pub mod addons;
#[cfg(feature = "console")]
pub mod api;
pub mod app;
pub mod auth;
pub mod auto_external_ip;
pub mod call;
pub mod call_errors;
pub mod callrecord;
pub mod config;
pub mod config_store;
#[cfg(feature = "console")]
pub mod console;
pub mod handler;
pub mod license;
pub mod log_reload;
pub mod log_viewer;

pub use rustpbx_http_util as http_util;
pub use rustpbx_media as media;
pub mod metrics;
pub use rustpbx_models as models;
pub mod observability;
pub mod outbound;
pub mod preflight;
pub mod proxy;
pub mod rwi;
pub mod shutdown;
pub use rustpbx_sipflow as sipflow;
pub use rustpbx_storage as storage;
pub mod sip_telemetry;
pub mod stats_log;
pub mod tls_reloader;
pub mod tts;
pub mod untracked_tasks;
pub mod utils;
pub mod version;

#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
