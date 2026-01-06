use crate::config::CallRecordConfig;
use crate::storage::{Storage, StorageConfig};
use anyhow::{Context, Result};
use bytes::Bytes;
use std::path::PathBuf;

#[derive(Clone)]
pub struct CdrStorage {
    inner: Storage,
}

impl CdrStorage {
    pub fn new(storage: Storage) -> Self {
        Self { inner: storage }
    }

    pub fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    pub fn local_full_path(&self, path: &str) -> Option<PathBuf> {
        self.inner.local_path(path)
    }

    pub fn path_for_metadata(&self, path: &str) -> String {
        if self.is_local() {
            self.inner
                .local_path(path)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        } else {
            path.to_string()
        }
    }

    pub async fn write_bytes(&self, path: &str, bytes: &[u8]) -> Result<String> {
        self.inner.write(path, Bytes::copy_from_slice(bytes)).await?;
        Ok(path.to_string())
    }

    pub async fn read_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let bytes = self.inner.read(path).await?;
        Ok(bytes.to_vec())
    }

    pub async fn read_to_string(&self, path: &str) -> Result<String> {
        let bytes = self.read_bytes(path).await?;
        Ok(String::from_utf8(bytes).with_context(|| format!("decode UTF-8 for {}", path))?)
    }
}

pub fn resolve_storage(config: Option<&CallRecordConfig>) -> Result<Option<CdrStorage>> {
    match config {
        Some(CallRecordConfig::Local { root }) => {
            let storage_config = StorageConfig::Local { path: root.clone() };
            let storage = Storage::new(&storage_config)?;
            Ok(Some(CdrStorage::new(storage)))
        }
        Some(CallRecordConfig::S3 {
            vendor,
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            root,
            ..
        }) => {
            let storage_config = StorageConfig::S3 {
                vendor: vendor.clone(),
                bucket: bucket.clone(),
                region: region.clone(),
                access_key: access_key.clone(),
                secret_key: secret_key.clone(),
                endpoint: Some(endpoint.clone()),
                prefix: Some(root.clone()),
            };
            let storage = Storage::new(&storage_config)?;
            Ok(Some(CdrStorage::new(storage)))
        }
        Some(CallRecordConfig::Http { .. }) => Ok(None),
        None => Ok(None),
    }
}
