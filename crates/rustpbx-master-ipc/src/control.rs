//! Control-plane protocol messages (JSON frames on the control socketpair).
//!
//! fd passing (SCM_RIGHTS) happens only on the control channel, attached to
//! the very same `sendmsg` call as the first byte of a message.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// What kind of stream an fd-carrying `TcpConn` represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    SipTcp,
    SipTls,
    Http,
    HttpTls,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ControlMessage {
    // ── worker → master ──
    Hello {
        generation: u32,
        pid: u32,
        version: String,
    },
    Ready,
    DialogUp {
        call_id: String,
    },
    DialogDown {
        call_id: String,
    },
    Drained,
    Stats {
        active_dialogs: u64,
    },
    /// Periodic full-snapshot reconciliation: the worker's complete live
    /// dialog set. Lets the master rebuild its routing table after a master
    /// restart (the table is in-memory only).
    Snapshot {
        call_ids: Vec<String>,
    },
    Bye {
        reason: String,
    },

    // ── master → worker ──
    Activate,
    Standby,
    Drain {
        timeout_secs: u64,
    },
    Stop {
        timeout_secs: u64,
    },
    Ping {
        nonce: u64,
    },
    UdpBind {
        listener_id: u16,
        local: SocketAddr,
        external: Option<SocketAddr>,
    },
    TcpConn {
        conn_id: u64,
        kind: StreamKind,
        local: SocketAddr,
        peer: SocketAddr,
    },
}

/// One control frame = `u32 BE len | JSON bytes`.
pub fn encode_control(msg: &ControlMessage) -> std::io::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

pub fn decode_control(buf: &[u8]) -> std::io::Result<ControlMessage> {
    serde_json::from_slice(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
