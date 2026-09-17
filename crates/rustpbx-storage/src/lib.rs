use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use http::Method;
use object_store::{
    ObjectMeta, ObjectStore, ObjectStoreExt, PutOptions, aws::AmazonS3Builder,
    azure::MicrosoftAzureBuilder, gcp::GoogleCloudStorageBuilder, local::LocalFileSystem,
    path::Path as ObjectPath, signer::Signer,
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};

/// Hard cap for presigned URL lifetime: SigV4 (AWS S3 and S3-compatible
/// services such as Aliyun OSS / Tencent COS) reject signatures with
/// `X-Amz-Expires` above 7 days.
pub const MAX_PRESIGN_EXPIRY_SECS: u64 = 604_800;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum S3Vendor {
    #[default]
    AWS,
    GCP,
    Azure,
    Aliyun,
    Tencent,
    Minio,
    DigitalOcean,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StorageConfig {
    Local {
        path: String,
    },
    S3 {
        vendor: S3Vendor,
        #[serde(default)]
        bucket: String,
        #[serde(default)]
        region: String,
        /// Access key id. Omit (or leave empty) together with `secret_key` to
        /// use anonymous/public access; S3-compatible stores that don't require
        /// credentials then receive unsigned requests.
        #[serde(default)]
        access_key: Option<String>,
        /// Secret access key. Omit (or leave empty) together with `access_key`
        /// for anonymous/public access.
        #[serde(default)]
        secret_key: Option<String>,
        endpoint: Option<String>,
        prefix: Option<String>,
    },
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig::Local {
            path: "storage".to_string(),
        }
    }
}

/// Normalize an optional credential: trims whitespace and treats an empty
/// string as absent.
fn normalize_credential(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Normalize an access/secret key pair shared by the AWS-style and Azure
/// backends. Returns `Some((access_key, secret_key))` when both are set,
/// `None` for anonymous/public access, and an error when only one of the two
/// is provided (a partial credential pair is a misconfiguration).
fn normalize_s3_credentials(
    access_key: Option<String>,
    secret_key: Option<String>,
) -> Result<Option<(String, String)>> {
    match (access_key, secret_key) {
        (Some(access_key), Some(secret_key)) => Ok(Some((access_key, secret_key))),
        (None, None) => Ok(None),
        _ => anyhow::bail!(
            "access_key and secret_key must both be set, or neither for anonymous access"
        ),
    }
}

#[derive(Clone)]
pub struct Storage {
    inner: Arc<dyn ObjectStore>,
    prefix: String,
    is_local: bool,
    local_root: Option<PathBuf>,
    /// Present for remote stores implementing [`object_store::signer::Signer`]
    /// (S3/S3-compatible, GCS, Azure). Enables offline presigned URL
    /// generation with the store's own credentials.
    signer: Option<Arc<dyn Signer>>,
    /// `(endpoint, bucket)` of the S3-compatible backend, used to map stored
    /// public-style URLs back to object keys.
    s3_info: Option<(Option<String>, String)>,
}

impl Storage {
    pub fn new(config: &StorageConfig) -> Result<Self> {
        match config {
            StorageConfig::Local { path } => {
                let root = PathBuf::from(path);
                std::fs::create_dir_all(&root)
                    .with_context(|| format!("create storage directory {}", path))?;
                let store = LocalFileSystem::new_with_prefix(&root)?;
                Ok(Self {
                    inner: Arc::new(store),
                    prefix: "".to_string(),
                    is_local: true,
                    local_root: Some(root),
                    signer: None,
                    s3_info: None,
                })
            }
            StorageConfig::S3 {
                vendor,
                bucket,
                region,
                access_key,
                secret_key,
                endpoint,
                prefix,
            } => {
                let endpoint = endpoint
                    .as_deref()
                    .map(str::trim)
                    .filter(|endpoint| !endpoint.is_empty())
                    .map(str::to_string);
                let bucket = bucket.trim_matches('/').to_string();
                let access_key = normalize_credential(access_key);
                let secret_key = normalize_credential(secret_key);
                let (inner, signer): (Arc<dyn ObjectStore>, Option<Arc<dyn Signer>>) = match vendor
                {
                    S3Vendor::AWS
                    | S3Vendor::Aliyun
                    | S3Vendor::Tencent
                    | S3Vendor::Minio
                    | S3Vendor::DigitalOcean => {
                        let credentials =
                            normalize_s3_credentials(access_key.clone(), secret_key.clone())?;
                        let mut builder = AmazonS3Builder::new()
                            .with_bucket_name(&bucket)
                            .with_region(region);
                        match credentials.as_ref() {
                            Some((access_key, secret_key)) => {
                                builder = builder
                                    .with_access_key_id(access_key)
                                    .with_secret_access_key(secret_key);
                            }
                            // No credentials → anonymous/public access; send
                            // unsigned requests instead of probing env/IMDS.
                            None => {
                                builder = builder.with_skip_signature(true);
                            }
                        }

                        if *vendor == S3Vendor::Aliyun {
                            builder = builder.with_virtual_hosted_style_request(true);
                        }
                        if let Some(ep) = endpoint.as_deref() {
                            builder = builder.with_endpoint(ep);
                            if ep.starts_with("http://") {
                                builder = builder.with_allow_http(true);
                            }
                        }
                        let store = Arc::new(builder.build()?);
                        let signer: Option<Arc<dyn Signer>> = credentials
                            .as_ref()
                            .map(|_| store.clone() as Arc<dyn Signer>);
                        (store as Arc<dyn ObjectStore>, signer)
                    }
                    S3Vendor::GCP => {
                        // GCS authenticates with a service-account key supplied
                        // via `secret_key`; `access_key` is accepted but ignored.
                        // Only the service-account key (or neither) is required.
                        if access_key.is_some() && secret_key.is_none() {
                            anyhow::bail!(
                                "gcp storage requires secret_key (service account key), \
                                 or neither key for anonymous access"
                            );
                        }
                        let mut builder = GoogleCloudStorageBuilder::new().with_bucket_name(&bucket);
                        match secret_key.as_ref() {
                            Some(service_account_key) => {
                                builder = builder.with_service_account_key(service_account_key);
                            }
                            None => {
                                builder = builder.with_skip_signature(true);
                            }
                        }
                        let store = Arc::new(builder.build()?);
                        let signer: Option<Arc<dyn Signer>> = secret_key
                            .as_ref()
                            .map(|_| store.clone() as Arc<dyn Signer>);
                        (store as Arc<dyn ObjectStore>, signer)
                    }
                    S3Vendor::Azure => {
                        let credentials =
                            normalize_s3_credentials(access_key, secret_key)?;
                        let mut builder =
                            MicrosoftAzureBuilder::new().with_container_name(&bucket);
                        match credentials.as_ref() {
                            Some((account, access_key)) => {
                                builder = builder.with_account(account).with_access_key(access_key);
                            }
                            None => {
                                builder = builder.with_skip_signature(true);
                            }
                        }
                        let store = Arc::new(builder.build()?);
                        let signer: Option<Arc<dyn Signer>> = credentials
                            .as_ref()
                            .map(|_| store.clone() as Arc<dyn Signer>);
                        (store as Arc<dyn ObjectStore>, signer)
                    }
                };

                Ok(Self {
                    inner,
                    prefix: prefix.clone().unwrap_or_default(),
                    is_local: false,
                    local_root: None,
                    signer,
                    s3_info: Some((endpoint, bucket)),
                })
            }
        }
    }

    fn normalize_path(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        if self.prefix.is_empty() {
            path.to_string()
        } else {
            format!("{}/{}", self.prefix.trim_end_matches('/'), path)
        }
    }

    fn object_path(&self, path: &str) -> Result<ObjectPath> {
        Ok(ObjectPath::parse(self.normalize_path(path))?)
    }

    pub async fn write(&self, path: &str, bytes: Bytes) -> Result<()> {
        if self.is_local
            && let Some(local_path) = self.local_path(path)
            && let Some(parent) = local_path.parent()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let object_path = self.object_path(path)?;
        self.inner.put(&object_path, bytes.into()).await?;
        Ok(())
    }

    pub async fn write_opts(&self, path: &str, bytes: Bytes, options: PutOptions) -> Result<()> {
        if self.is_local {
            return self.write(path, bytes).await;
        }
        let object_path = self.object_path(path)?;
        self.inner
            .put_opts(&object_path, bytes.into(), options)
            .await?;
        Ok(())
    }

    pub async fn read(&self, path: &str) -> Result<Bytes> {
        let object_path = self.object_path(path)?;
        let result = self.inner.get(&object_path).await?;
        let bytes = result.bytes().await?;
        Ok(bytes)
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        let object_path = self.object_path(path)?;
        self.inner.delete(&object_path).await?;
        Ok(())
    }

    /// Whether this backend can generate presigned URLs.
    pub fn supports_presign(&self) -> bool {
        self.signer.is_some()
    }

    /// Generate a presigned GET URL valid for `expires_in` (clamped to
    /// [`MAX_PRESIGN_EXPIRY_SECS`]). The URL embeds a SigV4-style signature
    /// computed offline from the configured credentials, so it works without
    /// this server running and grants read-only access to a single object.
    pub async fn presign_read_url(&self, path: &str, expires_in: Duration) -> Result<String> {
        let signer = self
            .signer
            .as_ref()
            .context("storage backend does not support presigned urls")?;
        let expires_in = expires_in.min(Duration::from_secs(MAX_PRESIGN_EXPIRY_SECS));
        let object_path = self.object_path(path)?;
        let url = signer
            .signed_url(Method::GET, &object_path, expires_in)
            .await?;
        Ok(url.to_string())
    }

    /// Map a previously stored public-style URL back to an object key managed
    /// by this store. Recognizes both `{endpoint}/{bucket}/{key}` (path-style,
    /// as produced when an endpoint is configured) and `s3://{bucket}/{key}`
    /// (when no endpoint is set). Returns `None` when the URL does not belong
    /// to this store.
    pub fn object_key_from_url(&self, raw_url: &str) -> Option<String> {
        let raw = raw_url.trim();
        if raw.is_empty() {
            return None;
        }
        let (endpoint, bucket) = self.s3_info.as_ref()?;
        let bucket = bucket.trim_matches('/');
        if let Some(ep) = endpoint
            .as_deref()
            .map(str::trim)
            .filter(|ep| !ep.is_empty())
        {
            let base = format!("{}/{}", ep.trim_end_matches('/'), bucket);
            if let Some(rest) = raw.strip_prefix(&base) {
                let key = rest.trim_start_matches('/');
                if !key.is_empty() {
                    return Some(key.to_string());
                }
            }
        }
        let s3_base = format!("s3://{}/", bucket);
        if let Some(rest) = raw.strip_prefix(&s3_base) {
            let key = rest.trim_start_matches('/');
            if !key.is_empty() {
                return Some(key.to_string());
            }
        }
        None
    }

    pub async fn list(&self, prefix: Option<&str>) -> Result<Vec<ObjectMeta>> {
        let prefix = prefix
            .map(|p| self.object_path(p))
            .unwrap_or_else(|| self.object_path(""))?;
        let mut stream = self.inner.list(Some(&prefix));
        let mut files = Vec::new();
        while let Some(item) = stream.next().await {
            let meta = item?;
            files.push(meta);
        }
        Ok(files)
    }

    pub fn is_local(&self) -> bool {
        self.is_local
    }

    pub fn local_path(&self, path: &str) -> Option<PathBuf> {
        self.local_root.as_ref().map(|root| {
            let cleaned: String = path
                .trim_start_matches('/')
                .split('/')
                .filter(|segment| !segment.is_empty() && *segment != ".")
                .map(|segment| {
                    if segment == ".." {
                        // Replace path traversal segments with safe placeholder
                        "_"
                    } else {
                        segment
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            root.join(cleaned)
        })
    }

    // Helper to upload a local file to storage (move)
    pub async fn upload_file(&self, local_path: &PathBuf, remote_path: &str) -> Result<()> {
        if self.is_local {
            let dest = self.local_path(remote_path).unwrap();
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // If src and dest are the same, do nothing
            if local_path != &dest {
                tokio::fs::rename(local_path, dest).await?;
            }
        } else {
            let data = tokio::fs::read(local_path).await?;
            self.write(remote_path, Bytes::from(data)).await?;
            tokio::fs::remove_file(local_path).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn s3_test_config(endpoint: Option<String>) -> StorageConfig {
        StorageConfig::S3 {
            vendor: S3Vendor::AWS,
            bucket: "recordings-bucket".to_string(),
            region: "oss-cn-hangzhou".to_string(),
            access_key: Some("test-access-key".to_string()),
            secret_key: Some("test-secret-key".to_string()),
            endpoint,
            prefix: None,
        }
    }

    #[tokio::test]
    async fn test_local_storage() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().to_str().unwrap().to_string();

        let config = StorageConfig::Local { path: path.clone() };
        let storage = Storage::new(&config)?;

        assert!(storage.is_local());

        // Test write
        let filename = "test.txt";
        let content = b"hello world";
        storage.write(filename, Bytes::from_static(content)).await?;

        // Test read
        let read_content = storage.read(filename).await?;
        assert_eq!(read_content, Bytes::from_static(content));

        // Test list
        let files = storage.list(Some("")).await?;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].location.as_ref(), filename);

        // Test delete
        storage.delete(filename).await?;
        let files = storage.list(Some("")).await?;
        assert!(files.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_upload_file_local() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().to_str().unwrap().to_string();

        let config = StorageConfig::Local { path: path.clone() };
        let storage = Storage::new(&config)?;

        // Create a dummy file outside storage
        let tmp_dir = tempdir()?;
        let src_path = tmp_dir.path().join("source.txt");
        tokio::fs::write(&src_path, b"source content").await?;

        // Upload (move) to storage
        let remote_path = "dest/file.txt";
        storage.upload_file(&src_path, remote_path).await?;

        // Verify file exists in storage
        let read_content = storage.read(remote_path).await?;
        assert_eq!(read_content, Bytes::from_static(b"source content"));

        // Verify source file is gone
        assert!(!src_path.exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_cdr_scenario() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().to_str().unwrap().to_string();
        let config = StorageConfig::Local { path };
        let storage = Storage::new(&config)?;

        let cdr_json = r#"{"call_id": "123", "duration": 60}"#;
        let filename = "cdr/2025/01/01/123.json";

        storage.write(filename, Bytes::from(cdr_json)).await?;

        let read_back = storage.read(filename).await?;
        assert_eq!(read_back, Bytes::from(cdr_json));
        Ok(())
    }

    #[tokio::test]
    async fn test_sipflow_scenario() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().to_str().unwrap().to_string();
        let config = StorageConfig::Local { path };
        let storage = Storage::new(&config)?;

        let sip_flow = "INVITE sip:...\n200 OK\nACK sip:...";
        let filename = "sipflow/123.txt";

        storage.write(filename, Bytes::from(sip_flow)).await?;

        let read_back = storage.read(filename).await?;
        assert_eq!(read_back, Bytes::from(sip_flow));
        Ok(())
    }

    #[tokio::test]
    async fn test_recorder_scenario() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().to_str().unwrap().to_string();
        let config = StorageConfig::Local { path };
        let storage = Storage::new(&config)?;

        let audio_data = vec![0u8; 1024];
        let filename = "recordings/123.wav";

        storage
            .write(filename, Bytes::from(audio_data.clone()))
            .await?;

        let read_back = storage.read(filename).await?;
        assert_eq!(read_back, Bytes::from(audio_data));
        Ok(())
    }

    #[tokio::test]
    async fn test_archive_scenario() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().to_str().unwrap().to_string();
        let config = StorageConfig::Local { path };
        let storage = Storage::new(&config)?;

        let compressed_data = vec![0x1f, 0x8b, 0x08, 0x00];
        let filename = "archive/2025-01-01-callrecords.gz";

        storage
            .write(filename, Bytes::from(compressed_data.clone()))
            .await?;

        let read_back = storage.read(filename).await?;
        assert_eq!(read_back, Bytes::from(compressed_data));
        Ok(())
    }

    #[tokio::test]
    async fn test_local_presign_unsupported() -> Result<()> {
        let dir = tempdir()?;
        let config = StorageConfig::Local {
            path: dir.path().to_str().unwrap().to_string(),
        };
        let storage = Storage::new(&config)?;

        assert!(!storage.supports_presign());
        assert!(
            storage
                .object_key_from_url("https://oss.example.com/bucket/a.wav")
                .is_none()
        );
        assert!(
            storage
                .presign_read_url("a.wav", Duration::from_secs(60))
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_presign_read_url() -> Result<()> {
        let storage = Storage::new(&s3_test_config(Some(
            "https://oss-cn-hangzhou.aliyuncs.com".to_string(),
        )))?;

        assert!(storage.supports_presign());
        let url = storage
            .presign_read_url("20260904/call.wav", Duration::from_secs(3600))
            .await?;
        assert!(url.starts_with(
            "https://oss-cn-hangzhou.aliyuncs.com/recordings-bucket/20260904/call.wav"
        ));
        assert!(url.contains("X-Amz-Signature="));
        assert!(url.contains("X-Amz-Expires=3600"));
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        Ok(())
    }

    #[tokio::test]
    async fn test_s3_presign_expires_clamped_to_sigv4_limit() -> Result<()> {
        let storage = Storage::new(&s3_test_config(None))?;
        // 90 days exceeds the SigV4 7-day limit and must be clamped.
        let url = storage
            .presign_read_url("20260904/call.wav", Duration::from_secs(90 * 24 * 3600))
            .await?;
        // No endpoint configured: object_store falls back to the default AWS
        // host, but the SigV4 query params must still be present and clamped.
        assert!(url.starts_with("https://"));
        assert!(url.contains("recordings-bucket"));
        assert!(url.contains("X-Amz-Expires=604800"));
        Ok(())
    }

    #[tokio::test]
    async fn test_object_key_from_url() -> Result<()> {
        let with_endpoint = Storage::new(&s3_test_config(Some(
            "https://oss-cn-hangzhou.aliyuncs.com".to_string(),
        )))?;
        assert_eq!(
            with_endpoint.object_key_from_url(
                "https://oss-cn-hangzhou.aliyuncs.com/recordings-bucket/20260904/call.wav"
            ),
            Some("20260904/call.wav".to_string())
        );
        assert_eq!(
            with_endpoint.object_key_from_url("s3://recordings-bucket/a/b.wav"),
            Some("a/b.wav".to_string())
        );
        assert_eq!(
            with_endpoint.object_key_from_url("https://other.example.com/recordings-bucket/a.wav"),
            None
        );
        assert_eq!(with_endpoint.object_key_from_url(""), None);

        let without_endpoint = Storage::new(&s3_test_config(None))?;
        assert_eq!(
            without_endpoint.object_key_from_url("s3://recordings-bucket/x.jsonl"),
            Some("x.jsonl".to_string())
        );
        // Without a configured endpoint the store cannot map path-style URLs
        // (the host is unknown), and foreign URLs must be rejected.
        assert_eq!(
            without_endpoint.object_key_from_url(
                "https://oss-cn-hangzhou.aliyuncs.com/recordings-bucket/a.wav"
            ),
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_anonymous_s3_without_credentials() -> Result<()> {
        let storage = Storage::new(&StorageConfig::S3 {
            vendor: S3Vendor::Minio,
            bucket: "public-bucket".to_string(),
            region: "us-east-1".to_string(),
            access_key: None,
            secret_key: None,
            endpoint: Some("http://127.0.0.1:9000".to_string()),
            prefix: None,
        })?;
        assert!(!storage.is_local());
        // Anonymous stores cannot sign, so callers fall back to raw public URLs.
        assert!(!storage.supports_presign());
        assert_eq!(
            storage.object_key_from_url("http://127.0.0.1:9000/public-bucket/a.wav"),
            Some("a.wav".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_blank_credentials_are_anonymous() -> Result<()> {
        let storage = Storage::new(&StorageConfig::S3 {
            vendor: S3Vendor::Minio,
            bucket: "public-bucket".to_string(),
            region: "us-east-1".to_string(),
            access_key: Some("  ".to_string()),
            secret_key: Some(String::new()),
            endpoint: Some("http://127.0.0.1:9000".to_string()),
            prefix: None,
        })?;
        assert!(!storage.supports_presign());
        Ok(())
    }

    #[tokio::test]
    async fn test_partial_credentials_rejected() {
        let result = Storage::new(&StorageConfig::S3 {
            vendor: S3Vendor::Minio,
            bucket: "b".to_string(),
            region: "us-east-1".to_string(),
            access_key: Some("only-access".to_string()),
            secret_key: None,
            endpoint: Some("http://127.0.0.1:9000".to_string()),
            prefix: None,
        });
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_gcp_anonymous_builds() -> Result<()> {
        let storage = Storage::new(&StorageConfig::S3 {
            vendor: S3Vendor::GCP,
            bucket: "public".to_string(),
            region: "us-central1".to_string(),
            access_key: None,
            secret_key: None,
            endpoint: None,
            prefix: None,
        })?;
        assert!(!storage.supports_presign());
        Ok(())
    }

    #[tokio::test]
    async fn test_gcp_access_key_without_secret_rejected() {
        // GCS has no access-key concept: a lone access key is an error.
        let result = Storage::new(&StorageConfig::S3 {
            vendor: S3Vendor::GCP,
            bucket: "bucket".to_string(),
            region: "us-central1".to_string(),
            access_key: Some("access".to_string()),
            secret_key: None,
            endpoint: None,
            prefix: None,
        });
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod aliyun_tests {
    use super::*;
    use crate::{Storage, StorageConfig};
    use std::time::Duration;

    #[tokio::test]
    async fn aliyun_empty_fields_keep_complete_endpoint() -> Result<()> {
        let storage = Storage::new(&StorageConfig::S3 {
            vendor: S3Vendor::Aliyun,
            bucket: String::new(),
            region: String::new(),
            endpoint: Some("https://test-bucket.oss-cn-beijing.aliyuncs.com".into()),
            access_key: Some("test".into()),
            secret_key: Some("test".into()),
            prefix: None,
        })?;
        for key in ["recordings/day/call.wav", "sipflow/day/call.jsonl"] {
            let signed = storage
                .presign_read_url(key, Duration::from_secs(60))
                .await?;
            assert!(signed.starts_with(&format!(
                "https://test-bucket.oss-cn-beijing.aliyuncs.com/{key}?"
            )));
        }
        Ok(())
    }

    #[tokio::test]
    async fn other_s3_vendors_preserve_path_style() -> Result<()> {
        for vendor in [
            S3Vendor::AWS,
            S3Vendor::Minio,
            S3Vendor::Tencent,
            S3Vendor::DigitalOcean,
        ] {
            let ep = "https://objects.example.com";
            let storage = Storage::new(&StorageConfig::S3 {
                vendor: vendor.clone(),
                bucket: "bucket".into(),
                region: "us-east-1".into(),
                endpoint: Some(ep.into()),
                access_key: Some("test".into()),
                secret_key: Some("test".into()),
                prefix: None,
            })?;
            let raw = "https://objects.example.com/bucket/nested/file.wav";
            assert!(
                storage
                    .presign_read_url("nested/file.wav", Duration::from_secs(60))
                    .await?
                    .starts_with(&format!("{raw}?"))
            );
        }
        Ok(())
    }
}
