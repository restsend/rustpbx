//! TLS Certificate Reloader Module
//!
//! This module provides hot-reload functionality for TLS certificates.
//! It allows updating HTTPS (axum) and SIP TLS (rsipstack) certificates
//! without restarting the server.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Trait for HTTPS certificate reloader (axum-server)
pub trait HttpsReloader: Send + Sync {
    /// Reload HTTPS certificate from PEM files
    fn reload_https(
        self: &Self,
        cert_path: &str,
        key_path: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>;
}

/// Trait for SIP TLS certificate reloader (rsipstack)
pub trait SipTlsReloader: Send + Sync {
    /// Reload SIP TLS certificate from PEM data
    fn reload_sip_tls(&self, cert_pem: Vec<u8>, key_pem: Vec<u8>) -> anyhow::Result<()>;
}

/// Unified TLS Reloader Registry
///
/// Manages both HTTPS and SIP TLS reloaders, allowing ACME addon
/// to trigger certificate reloads after auto-renewal.
pub struct TlsReloaderRegistry {
    https: Arc<RwLock<Option<Arc<dyn HttpsReloader>>>>,
    sip_tls: Arc<RwLock<Option<Arc<dyn SipTlsReloader>>>>,
}

impl TlsReloaderRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            https: Arc::new(RwLock::new(None)),
            sip_tls: Arc::new(RwLock::new(None)),
        }
    }

    /// Register an HTTPS reloader
    pub async fn register_https(&self, reloader: Arc<dyn HttpsReloader>) {
        let mut guard = self.https.write().await;
        *guard = Some(reloader);
        info!("HTTPS TLS reloader registered");
    }

    /// Register a SIP TLS reloader
    pub async fn register_sip_tls(&self, reloader: Arc<dyn SipTlsReloader>) {
        let mut guard = self.sip_tls.write().await;
        *guard = Some(reloader);
        info!("SIP TLS reloader registered");
    }

    /// Check if HTTPS reloader is registered
    pub async fn has_https_reloader(&self) -> bool {
        self.https.read().await.is_some()
    }

    /// Check if SIP TLS reloader is registered
    pub async fn has_sip_tls_reloader(&self) -> bool {
        self.sip_tls.read().await.is_some()
    }

    /// Reload HTTPS certificate
    pub async fn reload_https(&self, cert_path: &str, key_path: &str) -> anyhow::Result<()> {
        let guard = self.https.read().await;
        if let Some(reloader) = guard.as_ref() {
            info!(
                "Reloading HTTPS certificate from {} and {}",
                cert_path, key_path
            );
            reloader.reload_https(cert_path, key_path).await?;
            info!("HTTPS certificate reloaded successfully");
            Ok(())
        } else {
            warn!("No HTTPS reloader registered, skipping reload");
            Err(anyhow::anyhow!("No HTTPS reloader registered"))
        }
    }

    /// Reload SIP TLS certificate
    pub async fn reload_sip_tls(&self, cert_pem: Vec<u8>, key_pem: Vec<u8>) -> anyhow::Result<()> {
        let guard = self.sip_tls.read().await;
        if let Some(reloader) = guard.as_ref() {
            info!("Reloading SIP TLS certificate");
            reloader.reload_sip_tls(cert_pem, key_pem)?;
            info!("SIP TLS certificate reloaded successfully");
            Ok(())
        } else {
            warn!("No SIP TLS reloader registered, skipping reload");
            Err(anyhow::anyhow!("No SIP TLS reloader registered"))
        }
    }
}

impl Default for TlsReloaderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrapper for axum-server's RustlsConfig to enable hot reload
pub struct AxumRustlsReloader {
    config: Arc<axum_server::tls_rustls::RustlsConfig>,
}

impl AxumRustlsReloader {
    /// Create a new reloader from an existing RustlsConfig
    pub fn new(config: Arc<axum_server::tls_rustls::RustlsConfig>) -> Self {
        Self { config }
    }
}

impl HttpsReloader for AxumRustlsReloader {
    fn reload_https(
        &self,
        cert_path: &str,
        key_path: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        let cert_path = cert_path.to_string();
        let key_path = key_path.to_string();
        let config = self.config.clone();
        Box::pin(async move {
            config
                .reload_from_pem_file(&cert_path, &key_path)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to reload HTTPS certificate: {}", e))
        })
    }
}

/// Wrapper for rsipstack's TlsListenerConnection to enable hot reload
pub struct RsipstackTlsReloader {
    listener: rsipstack::transport::TlsListenerConnection,
}

impl RsipstackTlsReloader {
    /// Create a new reloader from an existing TlsListenerConnection
    pub fn new(listener: rsipstack::transport::TlsListenerConnection) -> Self {
        Self { listener }
    }
}

impl SipTlsReloader for RsipstackTlsReloader {
    fn reload_sip_tls(&self, cert_pem: Vec<u8>, key_pem: Vec<u8>) -> anyhow::Result<()> {
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            self.listener
                .reload_tls_config(cert_pem, key_pem)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to reload SIP TLS certificate: {}", e))
        })
    }
}
