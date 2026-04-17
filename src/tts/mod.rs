//! Text-to-Speech (TTS) service for dynamic audio generation in IVR.
//!
//! Supports HTTP and CLI drivers, with local file caching.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::debug;

pub mod cli_driver;
pub mod http_driver;

use cli_driver::synthesize_cli;
use http_driver::synthesize_http;

fn default_tts_cache_dir() -> String {
    std::env::temp_dir()
        .join("rustpbx_tts_cache")
        .to_string_lossy()
        .to_string()
}

fn default_tts_cache_ttl_seconds() -> u64 {
    86400
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TtsConfig {
    #[serde(default = "default_tts_cache_dir")]
    pub cache_dir: String,
    #[serde(default = "default_tts_cache_ttl_seconds")]
    pub cache_ttl_seconds: u64,
    pub driver: TtsDriverConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TtsDriverConfig {
    Http(HttpTtsConfig),
    Cli(CliTtsConfig),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpTtsConfig {
    pub url: String,
    #[serde(default = "default_http_method")]
    pub method: String,
    #[serde(default = "default_param_name")]
    pub param_name: String,
    #[serde(default)]
    pub extra_params: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_output_format")]
    pub output_format: String,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_body_format")]
    pub body_format: BodyFormat,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CliTtsConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_output_format")]
    pub output_format: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    Query,
    Form,
    Json,
}

fn default_http_method() -> String {
    "GET".to_string()
}

fn default_param_name() -> String {
    "text".to_string()
}

fn default_output_format() -> String {
    "wav".to_string()
}

fn default_timeout_seconds() -> u64 {
    30
}

fn default_body_format() -> BodyFormat {
    BodyFormat::Query
}

/// Shared TTS service that synthesizes text into cached audio files.
pub struct TtsService {
    config: TtsConfig,
    client: reqwest::Client,
}

impl TtsService {
    pub fn new(config: TtsConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    pub fn with_client(config: TtsConfig, client: reqwest::Client) -> Self {
        Self { config, client }
    }

    /// Synthesize `text` into a local audio file path.
    /// Returns the cached path if it already exists and is fresh.
    pub async fn synthesize(&self, text: &str, voice: Option<&str>) -> Result<String> {
        let cache_key = self.cache_key(text, voice);
        let cache_path = self.cache_path(&cache_key);

        if self.is_cache_valid(&cache_path).await {
            debug!(cache_path = %cache_path, "TTS cache hit");
            return Ok(cache_path);
        }

        debug!(text = %text, voice = ?voice, "TTS synthesizing");

        match &self.config.driver {
            TtsDriverConfig::Http(cfg) => {
                let bytes = synthesize_http(cfg, &self.client, text, voice).await?;
                tokio::fs::create_dir_all(
                    Path::new(&cache_path).parent().unwrap_or(Path::new(".")),
                )
                .await?;
                tokio::fs::write(&cache_path, bytes).await?;
                Ok(cache_path)
            }
            TtsDriverConfig::Cli(cfg) => {
                tokio::fs::create_dir_all(
                    Path::new(&cache_path).parent().unwrap_or(Path::new(".")),
                )
                .await?;
                synthesize_cli(cfg, text, &cache_path).await?;
                Ok(cache_path)
            }
        }
    }

    fn cache_key(&self, text: &str, voice: Option<&str>) -> String {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        voice.hash(&mut hasher);
        let driver_hash = format!("{:?}", self.config.driver);
        driver_hash.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    fn cache_path(&self, cache_key: &str) -> String {
        let ext = match self.config.driver {
            TtsDriverConfig::Http(ref c) => c.output_format.clone(),
            TtsDriverConfig::Cli(ref c) => c.output_format.clone(),
        };
        Path::new(&self.config.cache_dir)
            .join(format!("{}.{}", cache_key, ext))
            .to_string_lossy()
            .to_string()
    }

    async fn is_cache_valid(&self, path: &str) -> bool {
        let path = Path::new(path);
        if !path.exists() {
            return false;
        }
        if self.config.cache_ttl_seconds == 0 {
            return true;
        }
        match tokio::fs::metadata(path).await {
            Ok(meta) => match meta.modified() {
                Ok(modified) => {
                    let ttl = std::time::Duration::from_secs(self.config.cache_ttl_seconds);
                    std::time::SystemTime::now()
                        .duration_since(modified)
                        .map(|age| age < ttl)
                        .unwrap_or(false)
                }
                Err(_) => true,
            },
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};

    fn make_wav_bytes() -> Vec<u8> {
        let mut tmp = tempfile::NamedTempFile::with_suffix(".wav").unwrap();
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 8000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer =
                hound::WavWriter::new(std::io::BufWriter::new(tmp.as_file_mut()), spec).unwrap();
            for _ in 0..8000 {
                writer.write_sample(0i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        std::fs::read(tmp.path()).unwrap()
    }

    #[tokio::test]
    async fn test_tts_cache_hit() {
        let cache_dir = tempfile::tempdir().unwrap();
        let config = TtsConfig {
            cache_dir: cache_dir.path().to_string_lossy().to_string(),
            cache_ttl_seconds: 3600,
            driver: TtsDriverConfig::Http(HttpTtsConfig {
                url: "http://localhost:9999/tts".to_string(),
                method: "GET".to_string(),
                param_name: "text".to_string(),
                extra_params: HashMap::new(),
                headers: HashMap::new(),
                output_format: "wav".to_string(),
                timeout_seconds: 5,
                body_format: BodyFormat::Query,
            }),
        };

        let service = TtsService::new(config);
        let path = service.cache_path(&service.cache_key("hello", Some("voice1")));

        // Pre-seed cache
        tokio::fs::create_dir_all(cache_dir.path()).await.unwrap();
        tokio::fs::write(&path, make_wav_bytes()).await.unwrap();

        let result = service.synthesize("hello", Some("voice1")).await.unwrap();
        assert_eq!(result, path);
    }

    #[tokio::test]
    async fn test_tts_http_synthesize_get() {
        let wav = make_wav_bytes();
        let wav_clone = wav.clone();

        let app = Router::new().route(
            "/tts",
            get(
                move |axum::extract::Query(params): axum::extract::Query<
                    HashMap<String, String>,
                >| {
                    let wav = wav_clone.clone();
                    async move {
                        assert_eq!(params.get("text"), Some(&"hello world".to_string()));
                        assert_eq!(params.get("voice"), Some(&"zh-CN".to_string()));
                        ([("content-type", "audio/wav")], wav)
                    }
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let cache_dir = tempfile::tempdir().unwrap();
        let config = TtsConfig {
            cache_dir: cache_dir.path().to_string_lossy().to_string(),
            cache_ttl_seconds: 3600,
            driver: TtsDriverConfig::Http(HttpTtsConfig {
                url: format!("http://127.0.0.1:{}/tts", port),
                method: "GET".to_string(),
                param_name: "text".to_string(),
                extra_params: {
                    let mut m = HashMap::new();
                    m.insert("voice".to_string(), "zh-CN".to_string());
                    m
                },
                headers: HashMap::new(),
                output_format: "wav".to_string(),
                timeout_seconds: 5,
                body_format: BodyFormat::Query,
            }),
        };

        let service = TtsService::new(config);
        let path = service.synthesize("hello world", None).await.unwrap();
        assert!(
            path.contains("rustpbx_tts_cache") || path.contains(cache_dir.path().to_str().unwrap())
        );

        let written = tokio::fs::read(&path).await.unwrap();
        assert_eq!(written, wav);
    }
}
