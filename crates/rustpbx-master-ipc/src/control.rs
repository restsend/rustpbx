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

/// SIP connection class on the master-side routing tables. Unlike
/// [`StreamKind`] (what the forwarded fd IS at accept time), this is what the
/// fd BECAME inside the worker: an `Http` fd only reports `ConnUp` after the
/// WebSocket upgrade produced a SIP channel, and plain fd-forwarded HTTP API
/// connections never report at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnKind {
    SipTcp,
    SipTls,
    Ws,
    Wss,
}

impl ConnKind {
    pub fn as_u8(self) -> u8 {
        match self {
            ConnKind::SipTcp => 1,
            ConnKind::SipTls => 2,
            ConnKind::Ws => 3,
            ConnKind::Wss => 4,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(ConnKind::SipTcp),
            2 => Some(ConnKind::SipTls),
            3 => Some(ConnKind::Ws),
            4 => Some(ConnKind::Wss),
            _ => None,
        }
    }

    /// Carries SIP as framed bytes on a stream (text frames for WS kinds).
    pub fn is_stream(self) -> bool {
        true
    }
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
    /// A live SIP connection registered inside this worker (ws upgrade
    /// completed / fd-forwarded SIP TCP/TLS stream bridged). Feeds the
    /// master's ConnTable: accept-time affinity + cross-gen relay routing.
    ConnUp {
        conn_id: u64,
        kind: ConnKind,
        peer: SocketAddr,
        local: SocketAddr,
    },
    /// The connection died inside this worker. Master evicts the ConnTable
    /// entry and fans `ConnClosed` out to every generation holding a virtual
    /// relay connection toward it (their locator/dialog state hangs on it).
    ConnDown {
        conn_id: u64,
    },
    /// This generation now holds a virtual relay connection toward
    /// `(kind, peer)` (the real connection lives in another generation).
    RelayPairOpen {
        kind: ConnKind,
        peer: SocketAddr,
    },
    /// The virtual relay connection toward `(kind, peer)` was dropped (dialog
    /// ended). The REAL connection is untouched.
    RelayPairClose {
        kind: ConnKind,
        peer: SocketAddr,
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
    /// Broadcast on DialogUp: `call_id` dialogs are owned by `generation`.
    /// Workers keep this mirror to decide relay-vs-local for messages read
    /// from REAL connections (mirror says another gen owns it → relay).
    DialogOwnerAnnounce {
        call_id: String,
        generation: u32,
    },
    /// Broadcast on DialogDown: drop the mirror entry everywhere.
    DialogOwnerGone {
        call_id: String,
    },
    /// Full mirror rebuild on (re)connect: all dialogs owned by OTHER
    /// generations. Mirrors the dialog Snapshot semantics — the routing
    /// tables are in-memory only, so a fresh link needs the full picture.
    MirrorSnapshot {
        entries: Vec<(String, u32)>,
    },
    /// The real connection `(kind, peer)` died in its owning generation.
    /// Holders of virtual relay connections toward it must tear theirs down
    /// (cancel token + serve-loop exit → TransportEvent::Closed → the
    /// existing TransportInspectorLocator unbinds registrations bound to the
    /// phantom flow and dialogs fail fast instead of hanging on timers).
    ConnClosed {
        kind: ConnKind,
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
