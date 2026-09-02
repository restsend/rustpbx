use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
use arc_swap::ArcSwap;
use bytes::Bytes;
use rsipstack::sip::{SipMessage, ToTypedHeader, prelude::HeadersExt};
use rsipstack::{transaction::endpoint::MessageInspector, transport::SipAddr};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

struct Backend(Option<Arc<dyn SipFlowBackend>>);

/// Maximum time a query-path flush may take before the query proceeds anyway
/// (degraded: the result may miss records still sitting in write buffers).
pub const QUERY_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Late-bound handle to the process-wide [`SipFlow`]. Call-record hooks are
/// assembled before the SIP server (and thus the SipFlow) exists; the handle
/// is filled in right after server construction. Hooks only run after calls
/// finish — long after the handle has been set.
pub type SipFlowSlot = Arc<std::sync::OnceLock<SipFlow>>;

/// Flush a [`SipFlow`] pipeline with the query-path deadline. On timeout the
/// caller should proceed with the (possibly stale) query and log a warning.
pub async fn flush_with_deadline(sipflow: &SipFlow) {
    if tokio::time::timeout(QUERY_FLUSH_TIMEOUT, sipflow.flush())
        .await
        .is_err()
    {
        metrics::counter!("sipflow_query_flush_timeout_total", "component" => "sipflow")
            .increment(1);
    }
}

/// Flush through a late-bound [`SipFlowSlot`] handle, falling back to the bare
/// backend when the handle has not been set (tests / backends without a
/// SipFlow wrapper).
pub async fn flush_hook_pipeline(slot: &SipFlowSlot, backend: &dyn SipFlowBackend) {
    match slot.get() {
        Some(sipflow) => flush_with_deadline(sipflow).await,
        None => {
            if tokio::time::timeout(QUERY_FLUSH_TIMEOUT, backend.flush())
                .await
                .is_err()
            {
                metrics::counter!("sipflow_query_flush_timeout_total", "component" => "sipflow")
                    .increment(1);
            }
        }
    }
}

struct SipFlowInner {
    shared_backend: ArcSwap<Backend>,
    inspectors: Vec<Box<dyn MessageInspector>>,
    local_addrs: Vec<String>,
    /// Number of SIP messages rejected by backend dispatch.
    dropped_count: AtomicU64,
}

/// True when the host part is the unspecified address (`0.0.0.0` / `::`),
/// i.e. a placeholder that carries no routing information.
#[inline]
fn is_unspecified_host(host_with_port: &rsipstack::sip::HostWithPort) -> bool {
    matches!(
        &host_with_port.host,
        rsipstack::sip::Host::IpAddr(ip) if ip.is_unspecified()
    )
}

#[derive(Clone)]
pub struct SipFlow {
    inner: Arc<SipFlowInner>,
}

impl SipFlow {
    pub fn backend(&self) -> Option<Arc<dyn SipFlowBackend>> {
        self.inner.shared_backend.load().0.clone()
    }

    /// Number of SIP messages rejected by backend dispatch. Exposed for
    /// call-record diagnostics.
    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped_count.load(Ordering::Relaxed)
    }

    pub fn new(
        backend: Option<Arc<dyn SipFlowBackend>>,
        inspectors: Vec<Box<dyn MessageInspector>>,
    ) -> Self {
        let shared_backend = ArcSwap::new(Arc::new(Backend(backend)));

        SipFlow {
            inner: Arc::new(SipFlowInner {
                shared_backend,
                inspectors,
                local_addrs: Vec::new(),
                dropped_count: AtomicU64::new(0),
            }),
        }
    }

    /// Atomically swap the backend used by subsequent records and queries.
    pub fn swap_backend(&self, new_backend: Arc<dyn SipFlowBackend>) {
        self.inner
            .shared_backend
            .store(Arc::new(Backend(Some(new_backend))));
    }

    /// Remove the backend entirely (disable sipflow at runtime).
    pub fn clear_backend(&self) {
        self.inner.shared_backend.store(Arc::new(Backend(None)));
    }

    /// Ultra-optimized record_sip with zero-copy where possible
    #[inline]
    pub fn record_sip(&self, is_outgoing: bool, msg: &SipMessage, addr: Option<&SipAddr>) {
        // Keep one backend snapshot for the whole record so a concurrent hot
        // reload cannot move this message between backends midway through.
        let Some(backend) = self.backend() else {
            return;
        };

        // Fast path: extract call_id header without full parsing
        let call_id_result = match msg {
            rsipstack::sip::SipMessage::Request(req) => req.call_id_header(),
            rsipstack::sip::SipMessage::Response(resp) => resp.call_id_header(),
        };

        if let Ok(id) = call_id_result {
            let call_id = id.value().to_string();

            // OPTIMIZATION: Zero-copy - use pre-sized allocation
            let payload = Self::message_to_bytes_fast(msg);

            // OPTIMIZATION: Pre-sized string allocation
            let (src_addr, dst_addr) =
                Self::extract_addrs_fast(is_outgoing, addr, msg, &self.inner.local_addrs);

            let item = SipFlowItem {
                timestamp: chrono::Utc::now().timestamp_micros() as u64,
                seq: 0,
                leg: None,
                msg_type: SipFlowMsgType::Sip,
                src_addr,
                dst_addr,
                payload,
            };

            if backend.record(Cow::Owned(call_id), item).is_err() {
                self.inner.dropped_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Fast path: convert message to Bytes without full string clone
    #[inline]
    fn message_to_bytes_fast(msg: &SipMessage) -> Bytes {
        // Use the standard to_string but let Bytes reuse the allocation
        let msg_str = msg.to_string();
        Bytes::from(msg_str)
    }

    /// Fast address extraction with pre-allocated strings
    #[inline]
    fn extract_addrs_fast(
        is_outgoing: bool,
        addr: Option<&SipAddr>,
        msg: &SipMessage,
        local_addrs: &[String],
    ) -> (String, String) {
        let mut src = String::with_capacity(64);
        let mut dst = String::with_capacity(64);

        // A wildcard address means the endpoint address could not actually be
        // determined (e.g. RFC 7118 `.invalid` WebSocket flow tokens in the
        // Request-URI, or a wildcard-bound listener). Recording
        // `0.0.0.0:<port>` poisons the call-flow diagnostics downstream, so
        // for outgoing messages such addresses are ignored and the recorder
        // falls back to the message-derived target (or leaves dst empty).
        let addr = if is_outgoing {
            addr.filter(|a| !is_unspecified_host(&a.addr))
        } else {
            addr
        };

        if let Some(addr) = addr {
            let addr_str = addr.addr.to_string();
            if is_outgoing {
                dst.push_str(&addr_str);
            } else {
                src.push_str(&addr_str);
            }
        } else if is_outgoing
            && let Ok(dest) = rsipstack::transport::SipConnection::get_destination(msg)
            && !dest.ip().is_unspecified()
        {
            dst.push_str(&dest.to_string());
        }

        if is_outgoing {
            // Outgoing messages: fill src (local) side
            if src.is_empty() && msg.is_request() {
                // Outgoing requests: local address from Via sent-by
                if let Ok(via) = msg.via_header() {
                    if let Ok(typed_via) = via.typed() {
                        src.push_str(&typed_via.uri.host_with_port.to_string());
                    }
                }
            }
            if src.is_empty() && !local_addrs.is_empty() {
                src.push_str(&local_addrs[0]);
            }
        } else {
            // Incoming messages: fill dst (local) side with the server's
            // actual listening address, NOT the SIP Request-URI.
            if dst.is_empty() && !local_addrs.is_empty() {
                dst.push_str(&local_addrs[0]);
            }
            // Last resort fallback: for incoming responses, Via header can
            // tell us where the response was sent (our address).
            if dst.is_empty() {
                if let SipMessage::Response(resp) = msg {
                    if let Some(via_addr) = resp.via_received() {
                        dst.push_str(&via_addr.to_string());
                    } else if let Ok(via) = resp.via_header() {
                        if let Ok(typed_via) = via.typed() {
                            dst.push_str(&typed_via.uri.host_with_port.to_string());
                        }
                    }
                }
            }
        }

        (src, dst)
    }

    /// Flush the backend pipeline so all accepted messages are persisted
    /// before post-call uploads or queries.
    pub async fn flush(&self) {
        if let Some(ref backend) = (*self.inner.shared_backend.load()).0 {
            let _ = backend.flush().await;
        }
    }
}

impl MessageInspector for SipFlow {
    fn before_send(&self, msg: SipMessage, dest: Option<&SipAddr>) -> SipMessage {
        self.record_sip(true, &msg, dest);
        let mut modified_msg = msg;
        for inspector in &self.inner.inspectors {
            modified_msg = inspector.before_send(modified_msg, dest);
        }
        modified_msg
    }

    fn after_received(&self, msg: SipMessage, from: Option<&SipAddr>) -> SipMessage {
        self.record_sip(false, &msg, from);
        let mut modified_msg = msg;
        for inspector in &self.inner.inspectors {
            modified_msg = inspector.after_received(modified_msg, from);
        }
        modified_msg
    }
}

pub struct SipFlowBuilder {
    inspectors: Vec<Box<dyn MessageInspector>>,
    backend: Option<Arc<dyn SipFlowBackend>>,
    local_addrs: Vec<String>,
}

impl SipFlowBuilder {
    pub fn new() -> Self {
        Self {
            inspectors: Vec::new(),
            backend: None,
            local_addrs: Vec::new(),
        }
    }

    pub fn with_backend(mut self, backend: Arc<dyn SipFlowBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    pub fn register_inspector(mut self, inspector: Box<dyn MessageInspector>) -> Self {
        self.inspectors.push(inspector);
        self
    }

    /// Set the server's local listening addresses (e.g. ["0.0.0.0:5060", "0.0.0.0:15060"]).
    /// Used as the dst_addr for received messages and src_addr for sent messages.
    pub fn with_local_addrs(mut self, addrs: Vec<String>) -> Self {
        self.local_addrs = addrs;
        self
    }

    pub fn build(self) -> SipFlow {
        let mut flow = SipFlow::new(self.backend, self.inspectors);
        // SAFETY: inner is behind Arc but we just created it, no other references exist.
        let inner =
            Arc::get_mut(&mut flow.inner).expect("SipFlow inner uniquely held during build");
        inner.local_addrs = self.local_addrs;
        flow
    }
}

impl Default for SipFlowBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SipFlowSubdirs;
    use crate::sipflow::backend::local::LocalBackend;

    const CALL_ID: &str = "sipflow-flush-regression-call";

    fn sip_request(call_id: &str, cseq: u32) -> rsipstack::sip::SipMessage {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK{cseq}\r\n\
             From: <sip:alice@example.com>;tag=tag{cseq}\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        );
        rsipstack::sip::SipMessage::try_from(raw).expect("valid SIP request")
    }

    /// flush_count / flush_interval are set high so nothing is persisted
    /// unless an explicit flush drains the pipeline.
    fn test_backend(root: &std::path::Path) -> Arc<dyn SipFlowBackend> {
        Arc::new(
            LocalBackend::new(
                root.to_string_lossy().into_owned(),
                SipFlowSubdirs::None,
                1000,
                3600,
                128,
                None,
                2,
                false,
                16000,
                false,
            )
            .expect("local backend"),
        )
    }

    async fn query_count(backend: &Arc<dyn SipFlowBackend>, call_id: &str) -> usize {
        let now = chrono::Local::now();
        backend
            .query_flow(
                call_id,
                now - chrono::Duration::hours(1),
                now + chrono::Duration::hours(1),
            )
            .await
            .expect("query flow")
            .len()
    }

    /// CDR hooks and query endpoints flush through `SipFlow::flush`, which
    /// must make every message accepted by the backend visible to readers.
    #[tokio::test]
    async fn sipflow_flush_makes_backend_records_visible() {
        let dir = tempfile::tempdir().unwrap();
        let backend = test_backend(dir.path());
        let sipflow = SipFlow::new(Some(backend.clone()), Vec::new());

        for cseq in 1..=5u32 {
            sipflow.record_sip(false, &sip_request(CALL_ID, cseq), None);
        }

        // Simulate the post-call query path: flush exactly like
        // SipFlowUploadHook / the endpoints do (via the SipFlow wrapper).
        sipflow.flush().await;

        assert_eq!(
            query_count(&backend, CALL_ID).await,
            5,
            "every recorded message must be visible after SipFlow::flush"
        );
    }

    /// Hot-reload sends records made before the swap to the old backend and
    /// subsequent records directly to the new backend.
    #[tokio::test]
    async fn flush_after_swap_backend_targets_new_backend() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let b1 = test_backend(dir1.path());
        let sipflow = SipFlow::new(Some(b1.clone()), Vec::new());

        sipflow.record_sip(false, &sip_request(CALL_ID, 1), None);

        let b2 = test_backend(dir2.path());
        sipflow.swap_backend(b2.clone());
        for cseq in 2..=4u32 {
            sipflow.record_sip(false, &sip_request(CALL_ID, cseq), None);
        }

        sipflow.flush().await;
        b1.flush().await.unwrap();

        assert_eq!(query_count(&b1, CALL_ID).await, 1);
        assert_eq!(
            query_count(&b2, CALL_ID).await,
            3,
            "records after a hot reload must use the new backend"
        );
    }

    /// Production fault (callee-initiated BYE toward a JsSIP WebSocket
    /// caller): the outgoing BYE's Request-URI is an unresolvable
    /// `*.invalid` flow token. Without a transport-provided address the
    /// recorder used to fall back to a wildcard destination (`0.0.0.0:5060`)
    /// which corrupted the call flow view. The fallback must yield an empty
    /// dst instead, and a real transport address must always win.
    #[test]
    fn outgoing_request_dst_never_wildcard_and_addr_param_wins() {
        let invalid_bye = rsipstack::sip::SipMessage::try_from(
            "BYE sip:i2bkchck@7i94k9e6mr86.invalid;transport=WS;ob SIP/2.0\r\n\
             Via: SIP/2.0/UDP 116.62.250.247:15060;branch=z9hG4bKJRWrkRJNwtk7;rport\r\n\
             From: <sip:+17746371298@58.246.19.74:6988>;tag=y2Ty717d\r\n\
             To: <sip:xwork_5g_test@pbx.test.weixuntech-inc.com>;tag=tbjrt9l1te\r\n\
             Call-ID: q6cb2m7hj1j6gpsb6oif\r\n\
             CSeq: 2631 BYE\r\n\
             Content-Length: 0\r\n\r\n",
        )
        .expect("valid BYE");

        // No transport address (legacy affinity path): dst stays empty
        // instead of degenerating into a wildcard default.
        let (src, dst) = SipFlow::extract_addrs_fast(true, None, &invalid_bye, &[]);
        assert_eq!(
            dst, "",
            "unresolvable `.invalid` target must not become a wildcard dst"
        );
        assert_eq!(
            src, "116.62.250.247:15060",
            "src still comes from the Via sent-by"
        );

        // Simulated legacy garbage (wildcard listener as destination) — the
        // recorder must reject it as well.
        let wildcard = rsipstack::transport::SipAddr {
            r#type: Some(rsipstack::sip::transport::Transport::Udp),
            addr: rsipstack::sip::HostWithPort {
                host: rsipstack::sip::Host::IpAddr(std::net::IpAddr::V4(
                    std::net::Ipv4Addr::UNSPECIFIED,
                )),
                port: Some(5060.into()),
            },
        };
        let (_, dst) = SipFlow::extract_addrs_fast(true, Some(&wildcard), &invalid_bye, &[]);
        assert_eq!(
            dst, "",
            "wildcard transport address must not be recorded as dst"
        );

        // The real flow address (WebSocket remote peer) is recorded verbatim.
        let flow_addr = rsipstack::transport::SipAddr {
            r#type: Some(rsipstack::sip::transport::Transport::Wss),
            addr: rsipstack::sip::HostWithPort {
                host: rsipstack::sip::Host::IpAddr("112.64.233.138".parse().unwrap()),
                port: Some(7318.into()),
            },
        };
        let (_, dst) = SipFlow::extract_addrs_fast(true, Some(&flow_addr), &invalid_bye, &[]);
        assert_eq!(dst, "112.64.233.138:7318");
    }
}
