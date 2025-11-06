use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use object_store::{ObjectStore, path::Path as ObjectPath};

use crate::config::CallRecordConfig;

use super::build_object_store_from_s3;

#[derive(Clone)]
pub enum CdrStorage {
    Local {
        root: PathBuf,
    },
    S3 {
        store: Arc<dyn ObjectStore>,
        root: String,
    },
}

impl CdrStorage {
    fn resolve_local_path(&self, path: &str) -> PathBuf {
        match self {
            CdrStorage::Local { root } => {
                let candidate = Path::new(path);
                if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    let trimmed = path.trim_start_matches('/');
                    root.join(trimmed)
                }
            }
            _ => PathBuf::from(path),
        }
    }

    fn normalize_s3_key(&self, path: &str) -> String {
        match self {
            CdrStorage::S3 { root, .. } => {
                let trimmed_root = root.trim_matches('/');
                let trimmed_path = path.trim_matches('/');
                if trimmed_root.is_empty() {
                    trimmed_path.to_string()
                } else if trimmed_path.is_empty() {
                    trimmed_root.to_string()
                } else if trimmed_path.starts_with(trimmed_root) {
                    trimmed_path.to_string()
                } else {
                    format!("{}/{}", trimmed_root, trimmed_path)
                }
            }
            _ => path.to_string(),
        }
    }

    fn object_path(&self, path: &str) -> ObjectPath {
        ObjectPath::from(self.normalize_s3_key(path))
    }

    pub fn is_local(&self) -> bool {
        matches!(self, CdrStorage::Local { .. })
    }

    pub fn local_full_path(&self, path: &str) -> Option<PathBuf> {
        match self {
            CdrStorage::Local { .. } => Some(self.resolve_local_path(path)),
            _ => None,
        }
    }

    pub fn path_for_metadata(&self, path: &str) -> String {
        match self {
            CdrStorage::Local { .. } => {
                self.resolve_local_path(path).to_string_lossy().into_owned()
            }
            CdrStorage::S3 { .. } => self.normalize_s3_key(path),
        }
    }

    pub async fn write_bytes(&self, path: &str, bytes: &[u8]) -> Result<String> {
        match self {
            CdrStorage::Local { .. } => {
                let full_path = self.resolve_local_path(path);
                if let Some(parent) = full_path.parent() {
                    tokio::fs::create_dir_all(parent).await.with_context(|| {
                        format!("create transcript directory {}", parent.display())
                    })?;
                }
                tokio::fs::write(&full_path, bytes)
                    .await
                    .with_context(|| format!("write call record asset {}", full_path.display()))?;
                Ok(full_path.to_string_lossy().into_owned())
            }
            CdrStorage::S3 { store, .. } => {
                let key = self.normalize_s3_key(path);
                store
                    .put(
                        &self.object_path(path),
                        Bytes::copy_from_slice(bytes).into(),
                    )
                    .await
                    .with_context(|| format!("upload call record asset {}", key))?;
                Ok(key)
            }
        }
    }

    pub async fn read_bytes(&self, path: &str) -> Result<Vec<u8>> {
        match self {
            CdrStorage::Local { .. } => {
                let full_path = self.resolve_local_path(path);
                let data = tokio::fs::read(&full_path)
                    .await
                    .with_context(|| format!("read call record asset {}", full_path.display()))?;
                Ok(data)
            }
            CdrStorage::S3 { store, .. } => {
                let key = self.normalize_s3_key(path);
                let result = store
                    .get(&self.object_path(path))
                    .await
                    .with_context(|| format!("download call record asset {}", key))?;
                let bytes = result
                    .bytes()
                    .await
                    .with_context(|| format!("buffer call record asset {}", key))?;
                Ok(bytes.to_vec())
            }
        }
    }

    pub async fn read_to_string(&self, path: &str) -> Result<String> {
        let bytes = self.read_bytes(path).await?;
        Ok(String::from_utf8(bytes).with_context(|| format!("decode UTF-8 for {}", path))?)
    }
}

pub fn resolve_storage(config: Option<&CallRecordConfig>) -> Result<Option<CdrStorage>> {
    match config {
        Some(CallRecordConfig::Local { root }) => Ok(Some(CdrStorage::Local {
            root: PathBuf::from(root),
        })),
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
            let store = build_object_store_from_s3(
                vendor, bucket, region, access_key, secret_key, endpoint,
            )?;
            let normalized_root = root.trim_matches('/').to_string();
            Ok(Some(CdrStorage::S3 {
                store,
                root: normalized_root,
            }))
        }
        Some(CallRecordConfig::Http { .. }) => Ok(None),
        None => Ok(None),
    }
}
