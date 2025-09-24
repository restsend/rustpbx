use anyhow::{Result, anyhow};
use bytes::BytesMut;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use std::sync::RwLock;
use std::{io::IoSlice, path::PathBuf};
use tokio::io::AsyncReadExt;
use tokio::{fs::create_dir_all, io::AsyncWriteExt};
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

// Store datas in the cache
pub async fn store_in_cache_vectored(key: &str, data: &[impl AsRef<[u8]>]) -> Result<()> {
    ensure_cache_dir().await?;
    let path = get_cache_path(key)?;
    let tmp_path = path.with_extension(".tmp");
    let mut file = tokio::fs::File::create(tmp_path.clone()).await?;
    let io_slices = data
        .iter()
        .map(|d| IoSlice::new(d.as_ref()))
        .collect::<Vec<_>>();
    let n = file.write_vectored(&io_slices).await?;
    tokio::fs::rename(&tmp_path, &path).await?;
    info!("cache: Stored {} -> {} bytes", key, n);
    Ok(())
}

/// Retrieve data from the cache
pub async fn retrieve_from_cache(key: &str) -> Result<Vec<u8>> {
    let path = get_cache_path(key)?;

    if !tokio::fs::try_exists(&path).await? {
        return Err(anyhow!("Cache file not found for key: {}", key));
    }

    let data = tokio::fs::read(&path).await?;
    debug!(key, size = data.len(), "retrieved file from cache");
    Ok(data)
}

// Retrieve data from the cache with a buffer
pub async fn retrieve_from_cache_with_buffer(key: &str, buffer: &mut BytesMut) -> Result<()> {
    let path = get_cache_path(key)?;
    let mut file = tokio::fs::File::open(path).await?;
    let metadata = file.metadata().await?;
    let file_size = metadata.len() as usize;
    buffer.reserve(file_size);

    while file.read_buf(buffer).await? > 0 {}
    Ok(())
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

    #[tokio::test]
    async fn test_store_in_cache_v() -> Result<()> {
        // Test data as multiple slices
        let data1 = b"Hello, ";
        let data2 = b"world!";
        let data3 = b" This is a test.";
        let data_slices = [data1.as_slice(), data2.as_slice(), data3.as_slice()];

        let key = generate_cache_key("test_vectored_store", 16000, None, None);

        // Ensure the key doesn't exist initially
        delete_from_cache(&key).await.ok();
        assert!(!is_cached(&key).await?);

        // Store using vectored write
        store_in_cache_vectored(&key, &data_slices).await?;

        // Verify it was stored
        assert!(is_cached(&key).await?);

        // Retrieve and verify content
        let retrieved = retrieve_from_cache(&key).await?;
        let expected = [data1.as_slice(), data2.as_slice(), data3.as_slice()].concat();
        assert_eq!(retrieved, expected);

        // Clean up
        delete_from_cache(&key).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_store_in_cache_v_empty_slices() -> Result<()> {
        let empty_data: &[&[u8]] = &[];
        let key = generate_cache_key("test_empty_vectored", 16000, None, None);

        // Clean up first
        delete_from_cache(&key).await.ok();

        // Store empty data
        store_in_cache_vectored(&key, empty_data).await?;

        // Verify it was stored as empty file
        assert!(is_cached(&key).await?);
        let retrieved = retrieve_from_cache(&key).await?;
        assert_eq!(retrieved.len(), 0);

        // Clean up
        delete_from_cache(&key).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_retrieve_from_cache_with_buffer() -> Result<()> {
        // Test data
        let test_data = b"This is test data for buffer retrieval testing.";
        let key = generate_cache_key("test_buffer_retrieve", 16000, None, None);

        // Clean up first
        delete_from_cache(&key).await.ok();

        // Store test data using regular store
        store_in_cache(&key, &test_data.to_vec()).await?;

        // Retrieve using buffer method
        let mut buffer = BytesMut::new();
        retrieve_from_cache_with_buffer(&key, &mut buffer).await?;

        // Verify content
        assert_eq!(buffer.as_ref(), test_data);
        assert_eq!(buffer.len(), test_data.len());

        // Clean up
        delete_from_cache(&key).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_retrieve_from_cache_with_buffer_large_file() -> Result<()> {
        // Create larger test data (1MB)
        let large_data: Vec<u8> = vec![7; 1024 * 1024];
        let key = generate_cache_key("test_large_buffer", 16000, None, None);

        // Clean up first
        delete_from_cache(&key).await.ok();

        // Store large data
        store_in_cache(&key, &large_data).await?;

        // Retrieve using buffer method
        let mut buffer = BytesMut::new();
        retrieve_from_cache_with_buffer(&key, &mut buffer).await?;

        // Verify content
        assert_eq!(buffer.len(), large_data.len());
        assert_eq!(buffer.as_ref(), large_data.as_slice());

        // Clean up
        delete_from_cache(&key).await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_retrieve_from_cache_with_buffer_nonexistent() -> Result<()> {
        let nonexistent_key = generate_cache_key("nonexistent_file", 16000, None, None);
        let mut buffer = BytesMut::new();

        // Should fail for nonexistent file
        let result = retrieve_from_cache_with_buffer(&nonexistent_key, &mut buffer).await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_store_v_and_retrieve_buffer_integration() -> Result<()> {
        // Test integration between vectored store and buffer retrieve
        let data_parts = [
            b"Part 1: Hello".as_slice(),
            b", Part 2: World".as_slice(),
            b", Part 3: Integration Test!".as_slice(),
        ];
        let key = generate_cache_key("test_integration", 16000, None, None);

        // Clean up first
        delete_from_cache(&key).await.ok();

        // Store using vectored write
        store_in_cache_vectored(&key, &data_parts).await?;

        // Retrieve using buffer method
        let mut buffer = BytesMut::new();
        retrieve_from_cache_with_buffer(&key, &mut buffer).await?;

        // Verify the data was correctly concatenated
        let expected = data_parts.concat();
        assert_eq!(buffer.as_ref(), expected.as_slice());
        assert_eq!(buffer.len(), expected.len());

        // Also verify with regular retrieve for double-check
        let regular_retrieve = retrieve_from_cache(&key).await?;
        assert_eq!(regular_retrieve, expected);
        assert_eq!(buffer.as_ref(), regular_retrieve.as_slice());

        // Clean up
        delete_from_cache(&key).await?;
        Ok(())
    }
}
