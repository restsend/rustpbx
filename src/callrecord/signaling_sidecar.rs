//! Lightweight SIP signaling JSONL sidecar for call recording when the full
//! `[sipflow]` backend is not configured.
//!
//! Sessions with `[recording].enabled` register their Call-ID; matching SIP
//! messages are appended as JSON lines (same shape as SipFlow `export_jsonl`).

use dashmap::DashMap;
use rsipstack::{
    sip::{HeadersExt, SipMessage},
    transaction::endpoint::MessageInspector,
    transport::SipAddr,
};
use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tracing::warn;

#[derive(Clone)]
pub struct SignalingSidecar {
    inner: Arc<SidecarInner>,
}

struct SidecarInner {
    /// call_id → open jsonl path
    paths: DashMap<String, PathBuf>,
    seq: AtomicU64,
}

impl SignalingSidecar {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SidecarInner {
                paths: DashMap::new(),
                seq: AtomicU64::new(0),
            }),
        }
    }

    /// Begin capturing SIP for `call_id` into `path` (created/truncated).
    pub fn register(&self, call_id: impl Into<String>, path: PathBuf) {
        let call_id = call_id.into();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Truncate so a reused call-id does not append stale history.
        let _ = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path);
        self.inner.paths.insert(call_id, path);
    }

    pub fn unregister(&self, call_id: &str) -> Option<PathBuf> {
        self.inner.paths.remove(call_id).map(|(_, p)| p)
    }

    pub fn path_for(&self, call_id: &str) -> Option<PathBuf> {
        self.inner.paths.get(call_id).map(|p| p.clone())
    }

    fn append(&self, call_id: &str, line: &str) {
        let Some(path) = self.inner.paths.get(call_id) else {
            return;
        };
        match OpenOptions::new().create(true).append(true).open(path.value()) {
            Ok(mut file) => {
                if let Err(err) = writeln!(file, "{}", line) {
                    warn!(call_id, %err, "signaling sidecar write failed");
                }
            }
            Err(err) => {
                warn!(call_id, %err, "signaling sidecar open failed");
            }
        }
    }

    fn record_message(&self, is_outgoing: bool, msg: &SipMessage, addr: Option<&SipAddr>) {
        let call_id_header = match msg {
            SipMessage::Request(req) => req.call_id_header(),
            SipMessage::Response(resp) => resp.call_id_header(),
        };
        let Ok(id) = call_id_header else {
            return;
        };
        let call_id = id.value();
        if !self.inner.paths.contains_key(call_id) {
            return;
        }

        let payload = match msg {
            SipMessage::Request(req) => req.to_string(),
            SipMessage::Response(resp) => resp.to_string(),
        };
        let seq = self.inner.seq.fetch_add(1, Ordering::Relaxed);
        let (src_addr, dst_addr) = match (is_outgoing, addr) {
            (true, Some(a)) => (String::new(), a.to_string()),
            (false, Some(a)) => (a.to_string(), String::new()),
            _ => (String::new(), String::new()),
        };
        let obj = serde_json::json!({
            "timestamp": chrono::Utc::now().timestamp_micros() as u64,
            "seq": seq,
            "leg": serde_json::Value::Null,
            "msg_type": "sip",
            "src_addr": src_addr,
            "dst_addr": dst_addr,
            "payload": payload,
        });
        if let Ok(line) = serde_json::to_string(&obj) {
            self.append(call_id, &line);
        }
    }
}

impl MessageInspector for SignalingSidecar {
    fn before_send(&self, msg: SipMessage, dest: Option<&SipAddr>) -> SipMessage {
        self.record_message(true, &msg, dest);
        msg
    }

    fn after_received(&self, msg: SipMessage, from: Option<&SipAddr>) -> SipMessage {
        self.record_message(false, &msg, from);
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::transaction::endpoint::MessageInspector;

    fn invite_with_call_id(call_id: &str) -> SipMessage {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK1\r\n\
             From: <sip:alice@example.com>;tag=a\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        );
        SipMessage::try_from(raw.as_str()).expect("parse invite")
    }

    #[test]
    fn register_truncates_and_captures_only_registered_call_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sess.jsonl");
        std::fs::write(&path, b"stale-line\n").expect("seed");

        let sidecar = SignalingSidecar::new();
        sidecar.register("cid-1", path.clone());
        assert_eq!(sidecar.path_for("cid-1").as_deref(), Some(path.as_path()));

        // Truncated on register.
        let after_register = std::fs::read_to_string(&path).expect("read");
        assert!(after_register.is_empty());

        // Registered call is captured.
        let _ = sidecar.before_send(invite_with_call_id("cid-1"), None);
        let body = std::fs::read_to_string(&path).expect("read after capture");
        assert!(body.contains("INVITE"));
        assert!(body.contains("\"msg_type\":\"sip\""));

        // Unregistered call-id is ignored.
        let other = dir.path().join("other.jsonl");
        sidecar.register("cid-2", other.clone());
        let _ = sidecar.before_send(invite_with_call_id("cid-unknown"), None);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), body.len() as u64);

        let removed = sidecar.unregister("cid-1");
        assert_eq!(removed, Some(path));
        assert!(sidecar.path_for("cid-1").is_none());
    }
}
