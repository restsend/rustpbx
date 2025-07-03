use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::RwLock;
use tokio::fs::create_dir_all;
use tracing::{debug, info};

// Default cache directory
static DEFAULT_CACHE_DIR: &str = "/tmp/mediacache";

// Global cache configuration
static CACHE_CONFIG: Lazy<RwLock<CacheConfig>> = Lazy::new(|| {
    RwLock::new(CacheConfig {
        cache_dir: PathBuf::from(DEFAULT_CACHE_DIR),
    })
});

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub cache_dir: PathBuf,
}

/// Set the cache directory for the media cache
pub fn set_cache_dir(path: &str) -> Result<()> {
    let path = PathBuf::from(path);
    let mut config = CACHE_CONFIG
        .write()
        .map_err(|_| anyhow!("Failed to acquire write lock"))?;
    config.cache_dir = path;
    Ok(())
}

/// Get the current cache directory
pub fn get_cache_dir() -> Result<PathBuf> {
    let config = CACHE_CONFIG
        .read()
        .map_err(|_| anyhow!("Failed to acquire read lock"))?;
    Ok(config.cache_dir.clone())
}

/// Ensure the cache directory exists
pub async fn ensure_cache_dir() -> Result<()> {
    let cache_dir = get_cache_dir()?;

    if !cache_dir.exists() {
        debug!("Creating cache directory: {:?}", cache_dir);
        create_dir_all(&cache_dir).await?;
    }

    Ok(())
}

/// Generate a cache key from text or URL
pub fn generate_cache_key(
    input: &str,
    sample_rate: u32,
    speaker: Option<&String>,
    speed: Option<f32>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    match speaker {
        Some(speaker) => format!(
            "{}_{}_{}_{}",
            hex::encode(result),
            sample_rate,
            speaker,
            speed.unwrap_or(1.0)
        ),
        None => format!(
            "{}_{}_{}",
            hex::encode(result),
            sample_rate,
            speed.unwrap_or(1.0)
        ),
    }
}

/// Get the full path for a cached file
pub fn get_cache_path(key: &str) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;
    Ok(cache_dir.join(key).with_extension("pcm"))
}

/// Check if a file exists in the cache
pub async fn is_cached(key: &str) -> Result<bool> {
    let path = get_cache_path(key)?;
    Ok(tokio::fs::try_exists(&path).await?)
}

/// Store data in the cache
pub async fn store_in_cache(key: &str, data: &Vec<u8>) -> Result<()> {
    ensure_cache_dir().await?;
    let path = get_cache_path(key)?;
    tokio::fs::write(&path.with_extension(".tmp"), data).await?;
    tokio::fs::rename(&path.with_extension(".tmp"), &path).await?;
    info!("cache: Stored {} -> {} bytes", key, data.len());
    Ok(())
}

/// Retrieve data from the cache
pub async fn retrieve_from_cache(key: &str) -> Result<Vec<u8>> {
    let path = get_cache_path(key)?;

    if !tokio::fs::try_exists(&path).await? {
        return Err(anyhow!("Cache file not found for key: {}", key));
    }

    let data = tokio::fs::read(&path).await?;

    debug!("Retrieved file from cache with key: {}", key);
    Ok(data)
}

/// Delete a specific file from the cache
pub async fn delete_from_cache(key: &str) -> Result<()> {
    let path = get_cache_path(key)?;

    if tokio::fs::try_exists(&path).await? {
        tokio::fs::remove_file(path).await?;
        debug!("Deleted file from cache with key: {}", key);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_cache_operations() -> Result<()> {
        ensure_cache_dir().await?;

        // Generate a cache key
        let key = generate_cache_key("test_data", 8000, None, None);

        // Test storing data in cache
        let test_data = b"TEST DATA".to_vec();
        store_in_cache(&key, &test_data).await?;

        // Test if data is cached
        assert!(is_cached(&key).await?);

        // Test retrieving data from cache
        let retrieved_data = retrieve_from_cache(&key).await?;
        assert_eq!(retrieved_data, test_data);

        // Test deleting data from cache
        delete_from_cache(&key).await?;
        assert!(!is_cached(&key).await?);

        // Test clean cache
        let key2 = generate_cache_key("test_data2", 16000, None, None);
        store_in_cache(&key2, &test_data).await?;
        Ok(())
    }

    #[test]
    fn test_generate_cache_key() {
        let key1 = generate_cache_key("hello", 16000, None, None);
        let key2 = generate_cache_key("hello", 8000, None, None);
        let key3 = generate_cache_key("world", 16000, None, None);

        // Same input with different sample rates should produce different keys
        assert_ne!(key1, key2);

        // Different inputs with same sample rate should produce different keys
        assert_ne!(key1, key3);
    }
}
