use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::primitives::ByteStream;
use std::path::Path;

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

    pub async fn upload(&self, key: &str, path: &Path) -> Result<()> {
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
            .with_context(|| format!("S3 upload failed for key: {key}"))?;

        Ok(())
    }
}
