//! Config subset the master reads from the **same** config.toml the workers use.

use serde::Deserialize;
use std::net::SocketAddr;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ConfigSubset {
    pub http_addr: String,
    pub https_addr: Option<String>,
    pub proxy: ProxySubset,
    #[serde(default)]
    pub master: MasterSubset,
}

/// `[master]` section of the shared config.toml. All fields have defaults.
/// The admin API is served on a **unix socket** (access control = filesystem
/// permissions, 0o660) — no TCP port is opened.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct MasterSubset {
    /// Unix socket path for the admin API (status/test/reload).
    /// Defaults to `admin.sock` next to the worker IPC socket.
    pub admin_sock: Option<String>,
    /// Releases kept after GC (current + live-generation releases always kept).
    pub keep_releases: Option<usize>,
    /// Drain window handed to workers on rotation (informational for now).
    pub drain_timeout_secs: Option<u64>,
    /// Dialog-table hard cap (OOM protection; over cap = reject + counter).
    pub max_dialog_entries: Option<usize>,
}

impl MasterSubset {
    pub fn keep_releases(&self) -> usize {
        self.keep_releases.unwrap_or(3)
    }

    /// Default 500k entries (~a few tens of MB at ~100B/entry).
    pub fn max_dialog_entries(&self) -> usize {
        self.max_dialog_entries.unwrap_or(500_000)
    }

    /// Drain window on SIGTERM before the master exits with live generations.
    pub fn drain_timeout_secs(&self) -> u64 {
        self.drain_timeout_secs.unwrap_or(30)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpSlice {
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ProxySubset {
    pub addr: String,
    pub udp_port: Option<u16>,
    pub udp_ports: Option<Vec<u16>>,
    pub tcp_port: Option<u16>,
    pub rtp_start_port: Option<u16>,
    pub rtp_end_port: Option<u16>,
}

impl ConfigSubset {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read config: {e}"))?;
        toml::from_str(&raw).map_err(|e| format!("parse config: {e}"))
    }

    pub fn all_udp_ports(&self) -> Vec<u16> {
        let mut ports = self.proxy.udp_port.into_iter().collect::<Vec<_>>();
        if let Some(extra) = &self.proxy.udp_ports {
            for p in extra {
                if !ports.contains(p) { ports.push(*p); }
            }
        }
        ports
    }

    pub fn bind_ip(&self) -> Result<std::net::IpAddr, String> {
        self.proxy.addr.parse().map_err(|e| format!("proxy.addr: {e}"))
    }

    pub fn http_addr(&self) -> Result<SocketAddr, String> {
        self.http_addr.parse().map_err(|e| format!("http_addr: {e}"))
    }
}
