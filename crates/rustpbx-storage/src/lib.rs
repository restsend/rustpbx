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
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
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
                let (inner, signer): (Arc<dyn ObjectStore>, Option<Arc<dyn Signer>>) = match vendor
                {
                    S3Vendor::AWS
                    | S3Vendor::Aliyun
                    | S3Vendor::Tencent
                    | S3Vendor::Minio
                    | S3Vendor::DigitalOcean => {
                        let mut builder = AmazonS3Builder::new()
                            .with_bucket_name(&bucket)
                            .with_region(region)
                            .with_access_key_id(access_key)
                            .with_secret_access_key(secret_key);

                        if let Some(ep) = endpoint.as_deref() {
                            builder = builder.with_endpoint(ep);
                            if ep.starts_with("http://") {
                                builder = builder.with_allow_http(true);
                            }
                        }
                        let store = Arc::new(builder.build()?);
                        let signer = store.clone();
                        (store, Some(signer))
                    }
                    S3Vendor::GCP => {
                        let instance = Arc::new(
                            GoogleCloudStorageBuilder::new()
                                .with_bucket_name(&bucket)
                                .with_service_account_key(secret_key)
                                .build()?,
                        );
                        let signer = instance.clone();
                        (instance, Some(signer))
                    }
                    S3Vendor::Azure => {
                        let instance = Arc::new(
                            MicrosoftAzureBuilder::new()
                                .with_container_name(&bucket)
                                .with_account(access_key)
                                .with_access_key(secret_key)
                                .build()?,
                        );
                        let signer = instance.clone();
                        (instance, Some(signer))
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
            vendor: S3Vendor::Aliyun,
            bucket: "recordings-bucket".to_string(),
            region: "oss-cn-hangzhou".to_string(),
            access_key: "test-access-key".to_string(),
            secret_key: "test-secret-key".to_string(),
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
}
