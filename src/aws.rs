use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use bytes::Bytes;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

/// Files larger than this are uploaded using S3 multipart upload.
/// Single PUT is capped at 5 GB; we switch well before that.
const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024; // 100 MB

/// Size of each multipart chunk. Must be ≥ 5 MB (S3 minimum) for all but the last part.
const PART_SIZE: usize = 64 * 1024 * 1024; // 64 MB

pub struct S3Uploader {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl S3Uploader {
    pub async fn new(bucket: String, role_arn: &str) -> Result<Self> {
        // Use the IAM user credentials from the environment to call STS.
        let base_config = aws_config::defaults(BehaviorVersion::latest()).load().await;
        let sts = aws_sdk_sts::Client::new(&base_config);

        let assumed = sts
            .assume_role()
            .role_arn(role_arn)
            .role_session_name("google-photos-backup")
            .duration_seconds(12 * 3600) // 12 hours — enough for large Takeout archives
            .send()
            .await
            .context("Failed to assume upload role")?;

        let c = assumed
            .credentials
            .context("No credentials in AssumeRole response")?;

        let temp_creds = Credentials::new(
            c.access_key_id,
            c.secret_access_key,
            Some(c.session_token),
            None,
            "assumed-role",
        );

        let s3_config = aws_config::defaults(BehaviorVersion::latest())
            .credentials_provider(temp_creds)
            .load()
            .await;

        Ok(Self {
            client: aws_sdk_s3::Client::new(&s3_config),
            bucket,
        })
    }

    /// Returns top-level date prefixes (e.g. ["2024-01-01/", "2024-02-01/"]) sorted ascending.
    pub async fn list_backup_prefixes(&self) -> Result<Vec<String>> {
        let resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .delimiter("/")
            .send()
            .await
            .context("S3 ListObjectsV2 failed")?;

        let mut prefixes: Vec<String> = resp
            .common_prefixes()
            .iter()
            .filter_map(|p| p.prefix().map(|s| s.to_string()))
            .collect();

        prefixes.sort();
        Ok(prefixes)
    }

    /// Deletes all objects under `prefix` (e.g. "2024-01-01/").
    pub async fn delete_prefix(&self, prefix: &str) -> Result<usize> {
        let mut deleted_count = 0;
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(ref token) = continuation_token {
                req = req.continuation_token(token);
            }

            let page = req.send().await.context("S3 ListObjectsV2 failed")?;

            let keys: Vec<String> = page
                .contents()
                .iter()
                .filter_map(|o| o.key().map(|k| k.to_string()))
                .collect();

            for key in &keys {
                self.client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .send()
                    .await
                    .with_context(|| format!("S3 DeleteObject failed for key: {key}"))?;
                deleted_count += 1;
            }

            if page.is_truncated().unwrap_or(false) {
                continuation_token = page.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        Ok(deleted_count)
    }

    pub async fn upload(&self, key: &str, path: &Path) -> Result<()> {
        let file_size = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("Cannot stat file: {}", path.display()))?
            .len();

        if file_size <= MULTIPART_THRESHOLD {
            self.put_object(key, path).await
        } else {
            self.multipart_upload(key, path).await
        }
    }

    async fn put_object(&self, key: &str, path: &Path) -> Result<()> {
        let body = ByteStream::from_path(path)
            .await
            .with_context(|| format!("Cannot read file: {}", path.display()))?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .with_context(|| format!("S3 PutObject failed for key: {key}"))?;

        Ok(())
    }

    async fn multipart_upload(&self, key: &str, path: &Path) -> Result<()> {
        // 1. Initiate the multipart upload.
        let create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("Failed to initiate multipart upload for {key}"))?;

        let upload_id = create
            .upload_id()
            .context("No upload_id in CreateMultipartUpload response")?
            .to_string();

        // 2. Upload parts, aborting the multipart upload on any failure so we
        //    don't leave orphaned parts accumulating storage charges.
        match self.upload_parts(key, path, &upload_id).await {
            Ok(completed_parts) => {
                // 3. Complete.
                let completed = CompletedMultipartUpload::builder()
                    .set_parts(Some(completed_parts))
                    .build();

                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                    .with_context(|| format!("Failed to complete multipart upload for {key}"))?;

                Ok(())
            }
            Err(e) => {
                // Best-effort abort to clean up any uploaded parts.
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;

                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        key: &str,
        path: &Path,
        upload_id: &str,
    ) -> Result<Vec<CompletedPart>> {
        let mut file = File::open(path)
            .await
            .with_context(|| format!("Cannot open file: {}", path.display()))?;

        let mut completed_parts = Vec::new();
        let mut part_number = 1i32;
        let mut buf = vec![0u8; PART_SIZE];

        loop {
            // Fill the buffer fully (or until EOF).
            let mut bytes_read = 0;
            loop {
                let n = file.read(&mut buf[bytes_read..]).await?;
                if n == 0 {
                    break; // EOF
                }
                bytes_read += n;
                if bytes_read == PART_SIZE {
                    break; // Buffer full — more data may follow.
                }
            }

            if bytes_read == 0 {
                break; // Nothing left to upload.
            }

            let body = ByteStream::from(Bytes::copy_from_slice(&buf[..bytes_read]));

            let part = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(body)
                .send()
                .await
                .with_context(|| format!("Failed to upload part {part_number} of {key}"))?;

            let etag = part
                .e_tag()
                .context("No ETag in UploadPart response")?
                .to_string();

            completed_parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build(),
            );

            part_number += 1;

            if bytes_read < PART_SIZE {
                break; // Last part — we've read everything.
            }
        }

        Ok(completed_parts)
    }
}
