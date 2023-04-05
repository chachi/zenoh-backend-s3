//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt;

use aws_sdk_s3::model::{
    BucketLocationConstraint, CreateBucketConfiguration, Delete, Object, ObjectIdentifier,
};
use aws_sdk_s3::output::{
    CreateBucketOutput, DeleteObjectOutput, DeleteObjectsOutput, GetObjectOutput,
};
use aws_sdk_s3::{output::PutObjectOutput, types::ByteStream, Client};
use aws_sdk_s3::{Credentials, Endpoint, Region};
use aws_smithy_client::hyper_ext;
use uhlc::Timestamp;
use zenoh::prelude::{Encoding, KeyExpr};
use zenoh::value::Value;
use zenoh::Result as ZResult;
use zenoh_buffers::SplitBuffer;
use zenoh_core::zerror;
use zenoh_keyexpr::OwnedKeyExpr;

use crate::config::TlsClientConfig;
use crate::utils::{S3Key, S3Value};

/// Client to communicate with the S3 storage.
pub(crate) struct S3Client {
    client: Client,
    bucket: String,
    region: Option<String>,
}

impl S3Client {
    /// Creates a new instance of the [S3Client].
    ///
    /// # Arguments
    ///
    /// * `credentials`: credentials to communicate with the storage
    /// * `bucket`: name of the bucket/storage
    /// * `region`: region where the bucket/storage ought to be located
    /// * `endpoint`: the endpoint where the storage is located, either an AWS endpoint
    ///     (see https://docs.aws.amazon.com/general/latest/gr/s3.html) or a custom one if you are
    ///     setting a MinIO instance. If None then the default AWS endpoint resolver will attempt
    ///     to retrieve the endpoint based on the specified region.
    /// * `tls_config`: optional TlsClientConfig to enable TLS security.
    pub async fn new(
        credentials: Credentials,
        bucket: String,
        region: Option<String>,
        endpoint: Option<String>,
        tls_config: Option<TlsClientConfig>,
    ) -> Self {
        let mut config_loader =
            aws_config::ConfigLoader::default().credentials_provider(credentials);

        config_loader = match region {
            Some(ref region) => config_loader.region(Region::new(region.to_owned())),
            None => {
                // The region MUST be specified to perform a request to an S3 server when using the
                // aws_sdk_s3 crate. However when working with the MinIO S3 implementation (where
                // the region is not considered) then in the config file the region is optional and
                // we instead specify an empty region here below.
                log::debug!("Region not specified. Setting empty region...");
                config_loader.region(Region::new(""))
            }
        };

        config_loader = match endpoint {
            Some(endpoint) => config_loader.endpoint_resolver(Endpoint::immutable(
                endpoint.parse().expect("Invalid endpoint: "),
            )),
            None => {
                log::debug!("Endpoint not specified.");
                config_loader
            }
        };

        let config = &config_loader.load().await;

        let client = if let Some(tls_config) = tls_config {
            Client::from_conf_conn(
                config.into(),
                hyper_ext::Adapter::builder().build(tls_config.https_connector),
            )
        } else {
            Client::new(config)
        };

        S3Client {
            client,
            bucket: bucket.to_string(),
            region,
        }
    }

    /// Retrieves the object associated to the [key] specified.
    pub async fn get_object(&self, key: &str) -> ZResult<GetObjectOutput> {
        Ok(self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key.to_string())
            .send()
            .await?)
    }

    /// Performs a put operation on the storage on the key specified (which corresponds to the
    /// name of the file to be created) with the [Sample] provided.
    pub async fn put_object(
        &self,
        key: String,
        value: Value,
        timestamp: Timestamp,
    ) -> ZResult<PutObjectOutput> {
        let body = ByteStream::from(value.payload.contiguous().to_vec());
        let mut metadata: HashMap<String, String> = HashMap::new();
        metadata.insert("timestamp_uhlc".to_string(), timestamp.to_string());
        Ok(self
            .client
            .put_object()
            .bucket(self.bucket.to_owned())
            .key(key)
            .body(body)
            .set_content_encoding(Some(value.encoding.to_string()))
            .set_metadata(Some(metadata))
            .send()
            .await?)
    }

    /// Performs a DELETE operation on the key specified.
    pub async fn delete_object(&self, key: String) -> ZResult<DeleteObjectOutput> {
        Ok(self
            .client
            .delete_object()
            .bucket(self.bucket.to_owned())
            .key(key)
            .send()
            .await?)
    }

    /// Deletes the specified objects from the bucket.
    pub async fn delete_objects_in_bucket(
        &self,
        objects: Vec<Object>,
    ) -> ZResult<DeleteObjectsOutput> {
        if objects.is_empty() {
            return Ok(DeleteObjectsOutput::builder()
                .set_deleted(Some(vec![]))
                .build());
        }

        let mut object_identifiers: Vec<ObjectIdentifier> = vec![];

        for object in objects {
            let identifier = ObjectIdentifier::builder()
                .set_key(object.key().map(|x| x.to_string()))
                .build();
            object_identifiers.push(identifier);
        }

        let delete = Delete::builder()
            .set_objects(Some(object_identifiers))
            .build();

        Ok(self
            .client
            .delete_objects()
            .bucket(self.bucket.to_owned())
            .delete(delete)
            .send()
            .await?)
    }

    /// Asyncronically creates the bucket associated to this client upon construction on a new
    /// tokio runtime.
    /// Returns:
    /// - Ok(Some(CreateBucketOutput)) in case the bucket was successfully created
    /// - Ok(Some(None)) in case the `reuse_bucket` parameter is true and the bucket already exists
    ///     and is owned by you
    /// - Error in any other case
    #[tokio::main]
    pub async fn create_bucket(&self, reuse_bucket: bool) -> ZResult<Option<CreateBucketOutput>> {
        let constraint = self
            .region
            .as_ref()
            .map(|region| BucketLocationConstraint::from(region.as_str()));
        let cfg = CreateBucketConfiguration::builder()
            .set_location_constraint(constraint)
            .build();
        let result = self
            .client
            .create_bucket()
            .create_bucket_configuration(cfg)
            .bucket(self.bucket.to_owned())
            .send()
            .await;

        match result {
            Ok(output) => Ok(Some(output)),
            Err(aws_sdk_s3::types::SdkError::ServiceError { err, raw }) => {
                if err.is_bucket_already_owned_by_you() && reuse_bucket {
                    return Ok(None);
                };
                Err(zerror!("Couldn't associate bucket '{self}': {raw:?}").into())
            }
            Err(err) => Err(zerror!("Couldn't create or associate bucket '{self}': {err}.").into()),
        }
    }

    /// Deletes the bucket associated to this storage.
    ///
    /// In order to fulfill this operation, all the contained files in the bucket are deleted.
    pub async fn delete_bucket(&self) -> ZResult<()> {
        let objects = self.list_objects_in_bucket().await?;
        self.delete_objects_in_bucket(objects).await?;
        self.client
            .delete_bucket()
            .bucket(&self.bucket)
            .send()
            .await?;
        log::debug!("Deleted bucket '{}'.", self.bucket.to_owned());
        Ok(())
    }

    /// Lists all the objects contained in the bucket.
    pub async fn list_objects_in_bucket(&self) -> ZResult<Vec<Object>> {
        let response = self
            .client
            .list_objects_v2()
            .bucket(self.bucket.to_owned())
            .send()
            .await?;
        Ok(response.contents().unwrap_or_default().to_vec())
    }

    /// Utility function to retrieve the S3Value of an object from the S3 storage.
    pub async fn get_value_from_storage(&self, s3_key: S3Key) -> ZResult<S3Value> {
        let result = self.get_object(&s3_key.key).await;
        let output = result.map_err(|e| zerror!("Get operation failed: {e}"))?;
        let metadata = output.metadata().cloned();
        Ok(S3Value {
            key: s3_key,
            value: S3Client::extract_value_from_response(output).await?,
            metadata,
        })
    }

    /// Utility function to retrieve the intersecting objects on the S3 storage with a wild key
    /// expression.
    ///
    /// # Arguments
    ///
    /// * `client` - the [S3Client] allowing us to communicate with the S3 server
    /// * `key_expr` - the wild key expression we want to intersect against the stored keys
    /// * `prefix` - prefix from the configuration
    ///
    /// # Returns
    ///
    /// A vector of the intersecting objects contained in the storage or an error result. Note that the
    /// objects do not contain their actual paylod, this must be retrieved afterwards doing a proper
    /// request.
    pub async fn get_intersecting_objects(
        &self,
        key_expr: &OwnedKeyExpr,
        prefix: Option<String>,
    ) -> ZResult<Vec<Object>> {
        let mut intersecting_objects_metadata = Vec::new();
        let objects_metadata = self.list_objects_in_bucket().await?;
        for metadata in objects_metadata {
            let s3_key = S3Key::from_key(
                prefix.to_owned(),
                metadata
                    .key()
                    .ok_or_else(|| zerror!("Unable to retrieve key from object {:?}", metadata))?
                    .to_owned(),
            );
            if key_expr.intersects(KeyExpr::try_from(s3_key)?.as_ref()) {
                intersecting_objects_metadata.push(metadata);
            }
        }
        Ok(intersecting_objects_metadata)
    }

    /// Utility function to extract the [Value] from a result.
    ///
    /// # Arguments
    ///
    /// * `response`: response from the S3 server to a get object request.
    async fn extract_value_from_response(response: GetObjectOutput) -> ZResult<Value> {
        let encoding = response.content_encoding().map(|x| x.to_string());
        let bytes = response
            .body
            .collect()
            .await
            .map(|data| data.into_bytes())?;

        let value = match encoding {
            Some(encoding) => Encoding::try_from(encoding).map_or_else(
                |_| Value::from(Vec::from(bytes.to_owned())),
                |result| Value::from(Vec::from(bytes.to_owned())).encoding(result),
            ),
            None => Value::from(Vec::from(bytes)),
        };
        Ok(value)
    }
}

impl std::fmt::Display for S3Client {
    // It's sufficient to display the bucket name as we only have a single
    // bucket and a single storage for each client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.bucket)
    }
}
