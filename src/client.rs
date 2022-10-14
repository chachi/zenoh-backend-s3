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

use std::fmt;

use aws_sdk_s3::error::CreateBucketError;
use aws_sdk_s3::model::{
    BucketLocationConstraint, CreateBucketConfiguration, Delete, Object, ObjectIdentifier,
};
use aws_sdk_s3::output::{
    CreateBucketOutput, DeleteObjectOutput, DeleteObjectsOutput, GetObjectOutput,
};
use aws_sdk_s3::types::SdkError;
use aws_sdk_s3::{output::PutObjectOutput, types::ByteStream, Client};
use aws_sdk_s3::{Credentials, Endpoint, Region};
use aws_smithy_http::operation::Response;
use zenoh::sample::Sample;
use zenoh::Result as ZResult;
use zenoh_buffers::SplitBuffer;
use zenoh_core::zerror;

/// Client to communicate with the S3 storage.
pub struct S3Client {
    client: Client,
    bucket: String,
    region: Region,
    credentials: Credentials,
}

impl S3Client {
    /// Creates a new instance of the [S3Client].
    ///
    /// # Arguments
    ///
    /// * `credentials`: credentials to communicate with the storage
    /// * `region`: region where the bucket/storage ought to be located
    /// * `bucket`: name of the bucket/storage
    /// * `endpoint`: endpoint of the storage, for instance `http://localhost:8080`, it should be
    ///        indicated in the json5 configuration file.
    pub async fn new(
        credentials: Credentials,
        region: Region,
        bucket: String,
        endpoint: String,
    ) -> Self {
        let sdk_config = aws_config::ConfigLoader::default()
            .endpoint_resolver(Endpoint::immutable(
                endpoint.parse().expect("Invalid endpoint: "),
            ))
            .region(region.to_owned())
            .credentials_provider(credentials.to_owned())
            .load()
            .await;

        let client = Client::new(&sdk_config);
        S3Client {
            client,
            bucket: bucket.to_string(),
            region,
            credentials,
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
    pub async fn put_object(&self, key: String, sample: Sample) -> ZResult<PutObjectOutput> {
        let body = ByteStream::from(sample.payload.contiguous().to_vec());
        Ok(self
            .client
            .put_object()
            .bucket(self.bucket.to_owned())
            .key(key)
            .body(body)
            .set_content_encoding(Some(sample.encoding.to_string()))
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
    #[tokio::main]
    pub async fn create_bucket(&self) -> ZResult<CreateBucketOutput> {
        Ok(self.send_create_bucket_request().await?)
    }

    /// Asyncronically creates the bucket associated to this client upon construction on a new
    /// tokio runtime. If the creation failed because the bucket already exists but it's a bucket
    /// that is already owned by the client, then returns `Ok(None)` instead of a [ZError].
    #[tokio::main]
    pub async fn create_or_associate_bucket(&self) -> ZResult<Option<CreateBucketOutput>> {
        let result = self.send_create_bucket_request().await;
        match result {
            Ok(output) => Ok(Some(output)),
            Err(aws_sdk_s3::types::SdkError::ServiceError { err, raw }) => {
                if err.is_bucket_already_owned_by_you() {
                    return Ok(None);
                };
                Err(zerror!("Couldn't associate bucket '{self:?}': {raw:?}").into())
            }
            _ => Err(zerror!("Couldn't create or associate bucket '{self:?}'.").into()),
        }
    }

    /// Asyncronically verifies if the bucket associated to this client is owned by it.
    /// Returns `Ok(())` if the bucket is owned by the client, otherwise a `ZError` is returned.
    #[tokio::main]
    pub async fn verify_bucket_ownership(&self) -> ZResult<()> {
        let result = self
            .client
            .get_bucket_ownership_controls()
            .bucket(self.bucket.to_owned())
            .expected_bucket_owner(self.credentials.access_key_id())
            .send()
            .await;

        match result {
            Ok(_) => Ok(()),
            Err(aws_sdk_s3::types::SdkError::ServiceError { err, raw }) => match err.code() {
                Some("403") => Err(zerror!(
                    "The bucket '{self:?} is owned by a different account: {raw:?}"
                )
                .into()),
                _ => {
                    Err(zerror!("Unable to verify bucket ownership for {self:?}: {raw:?}.").into())
                }
            },
            Err(e) => Err(zerror!("Unable to verify bucket ownership for {self:?}: {e}.").into()),
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
    async fn list_objects_in_bucket(&self) -> ZResult<Vec<Object>> {
        let response = self
            .client
            .list_objects_v2()
            .bucket(self.bucket.to_owned())
            .send()
            .await?;
        Ok(response.contents().unwrap_or_default().to_vec())
    }

    async fn send_create_bucket_request(
        &self,
    ) -> Result<CreateBucketOutput, SdkError<CreateBucketError, Response>> {
        let constraint = BucketLocationConstraint::from(self.region.to_string().as_str());
        let cfg = CreateBucketConfiguration::builder()
            .location_constraint(constraint)
            .build();
        self.client
            .create_bucket()
            .create_bucket_configuration(cfg)
            .bucket(self.bucket.to_owned())
            .send()
            .await
    }
}

impl fmt::Debug for S3Client {
    // It's sufficient to display the bucket name as we only have a single
    // bucket and a single storage for each client.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("").field(&self.bucket).finish()
    }
}
