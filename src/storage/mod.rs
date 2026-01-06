use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use object_store::{
    ObjectMeta, ObjectStore, ObjectStoreExt, aws::AmazonS3Builder, azure::MicrosoftAzureBuilder,
    gcp::GoogleCloudStorageBuilder, local::LocalFileSystem, path::Path as ObjectPath,
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum S3Vendor {
    AWS,
    GCP,
    Azure,
    Aliyun,
    Tencent,
    Minio,
    DigitalOcean,
}

impl Default for S3Vendor {
    fn default() -> Self {
        S3Vendor::AWS
    }
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
                let store: Arc<dyn ObjectStore> = match vendor {
                    S3Vendor::AWS
                    | S3Vendor::Aliyun
                    | S3Vendor::Tencent
                    | S3Vendor::Minio
                    | S3Vendor::DigitalOcean => {
                        let mut builder = AmazonS3Builder::new()
                            .with_bucket_name(bucket)
                            .with_region(region)
                            .with_access_key_id(access_key)
                            .with_secret_access_key(secret_key);

                        if let Some(ep) = endpoint {
                            if !ep.is_empty() {
                                builder = builder.with_endpoint(ep);
                            }
                        }
                        Arc::new(builder.build()?)
                    }
                    S3Vendor::GCP => {
                        let instance = GoogleCloudStorageBuilder::new()
                            .with_bucket_name(bucket)
                            .with_service_account_key(secret_key)
                            .build()?;
                        Arc::new(instance)
                    }
                    S3Vendor::Azure => {
                        let instance = MicrosoftAzureBuilder::new()
                            .with_container_name(bucket)
                            .with_account(access_key)
                            .with_access_key(secret_key)
                            .build()?;
                        Arc::new(instance)
                    }
                };

                Ok(Self {
                    inner: store,
                    prefix: prefix.clone().unwrap_or_default(),
                    is_local: false,
                    local_root: None,
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

    fn object_path(&self, path: &str) -> ObjectPath {
        ObjectPath::from(self.normalize_path(path))
    }

    pub async fn write(&self, path: &str, bytes: Bytes) -> Result<()> {
        if self.is_local {
            if let Some(local_path) = self.local_path(path) {
                if let Some(parent) = local_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
            }
        }
        let object_path = self.object_path(path);
        self.inner.put(&object_path, bytes.into()).await?;
        Ok(())
    }

    pub async fn read(&self, path: &str) -> Result<Bytes> {
        let object_path = self.object_path(path);
        let result = self.inner.get(&object_path).await?;
        let bytes = result.bytes().await?;
        Ok(bytes)
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        let object_path = self.object_path(path);
        self.inner.delete(&object_path).await?;
        Ok(())
    }

    pub async fn list(&self, prefix: Option<&str>) -> Result<Vec<ObjectMeta>> {
        let prefix = prefix
            .map(|p| self.object_path(p))
            .unwrap_or_else(|| self.object_path(""));
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
        if let Some(root) = &self.local_root {
            Some(root.join(path.trim_start_matches('/')))
        } else {
            None
        }
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
}
