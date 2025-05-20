use super::{server::SipServerRef, ProxyModule};
use crate::config::ProxyConfig;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
    sync::Mutex,
};
use tracing::{error, info};

#[derive(Serialize, Deserialize, Clone)]
pub struct CdrMedia {
    pub owner: String,
    pub ssrc: u32,
    pub media_type: String,
    pub media_format: String,
    pub media_duration: Duration,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Cdr {
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: String,
    pub from_uri: String,
    pub to_uri: String,
    pub start_time: SystemTime,
    pub end_time: Option<SystemTime>,
    pub offer: Option<String>,
    pub answer: Option<String>,
    pub status_code: Option<u16>,
    pub status_reason: Option<String>,
    pub duration: Option<Duration>,
    pub media: Vec<CdrMedia>,
}

#[async_trait]
pub trait CdrBackend: Send + Sync {
    async fn record(
        &self,
        call_id: &str,
        from_tag: &str,
        to_tag: &str,
        from_uri: &str,
        to_uri: &str,
        offer: Option<String>,
        answer: Option<String>,
    ) -> Result<Cdr>;
    async fn save(&self, call_id: &str, cdr: Cdr) -> Result<()>;
}

pub struct JsonCdrBackend {
    pub path: PathBuf,
}

impl JsonCdrBackend {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl CdrBackend for JsonCdrBackend {
    async fn record(
        &self,
        call_id: &str,
        from_tag: &str,
        to_tag: &str,
        from_uri: &str,
        to_uri: &str,
        offer: Option<String>,
        answer: Option<String>,
    ) -> Result<Cdr> {
        Ok(Cdr {
            call_id: call_id.to_string(),
            from_tag: from_tag.to_string(),
            to_tag: to_tag.to_string(),
            from_uri: from_uri.to_string(),
            to_uri: to_uri.to_string(),
            start_time: SystemTime::now(),
            end_time: None,
            offer,
            answer,
            status_code: None,
            status_reason: None,
            duration: None,
            media: Vec::new(),
        })
    }

    async fn save(&self, call_id: &str, cdr: Cdr) -> Result<()> {
        let file_path = self.path.join(format!("{}.json", call_id));
        let file = File::create(file_path).await?;
        let mut writer = BufWriter::new(file);
        let json = serde_json::to_string_pretty(&cdr)?;
        writer.write_all(json.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

// Helper struct to track active calls
struct CallSession {
    cdr: Cdr,
    invite_time: SystemTime,
}

struct CdrModuleInner {
    backend: Box<dyn CdrBackend>,
    active_calls: Mutex<HashMap<String, CallSession>>,
}
#[derive(Clone)]
pub struct CdrModule {
    inner: Arc<CdrModuleInner>,
}

impl CdrModule {
    pub fn create(_server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = CdrModule::new(config);
        Ok(Box::new(module))
    }
    pub fn new(config: Arc<ProxyConfig>) -> Self {
        let path = PathBuf::from("cdr");
        if !path.exists() {
            match std::fs::create_dir_all(&path) {
                Ok(_) => info!("created cdr directory {}", path.display()),
                Err(e) => error!("failed to create cdr directory: {} {}", path.display(), e),
            }
        }
        let inner = Arc::new(CdrModuleInner {
            backend: Box::new(JsonCdrBackend::new(path)),
            active_calls: Mutex::new(HashMap::new()),
        });
        Self { inner }
    }
}

#[async_trait]
impl ProxyModule for CdrModule {
    fn name(&self) -> &str {
        "cdr"
    }
    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![]
    }

    async fn on_start(&mut self) -> Result<()> {
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }
}
