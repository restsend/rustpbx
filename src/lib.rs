pub mod addons;
#[cfg(feature = "console")]
pub mod api;
pub mod app;
pub mod auth;
pub mod auto_external_ip;
pub mod call;
pub mod callrecord;
pub mod config;
#[cfg(feature = "console")]
pub mod console;
pub mod handler;
pub mod license;

pub mod http_util;
pub mod media;
pub mod metrics;
pub mod models;
pub mod observability;
pub mod preflight;
pub mod proxy;
pub mod rwi;
pub mod sipflow;
pub mod storage;
pub mod tls_reloader;
pub mod tts;
pub mod utils;
pub mod version;

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_rustls_crypto_provider() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider for tests");
}
