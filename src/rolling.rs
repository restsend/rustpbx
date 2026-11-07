//! Rolling process handover for container deployments.
//!
//! Two processes (old release / new release) overlap for a bounded window.
//! Roles:
//!
//! * **Successor** (new container): starts first, hello-pings the
//!   predecessor over loopback while waiting for the public ports. Once the
//!   predecessor closes its listeners the successor owns every public port
//!   and serves all NEW traffic immediately.
//! * **Predecessor** (old container): on SIGTERM with a fresh successor
//!   heartbeat it performs the handover instead of a plain drain:
//!
//!   1. bind the loopback [`RollingConfig::predecessor_port`] listener
//!      (the full management router, which already carries the
//!      `/rolling/v1/relay_ingress` endpoint),
//!   2. push the handoff payload (live call-id manifest, optional
//!      registration dump) to the successor,
//!   3. swap the SIP UDP transports to loopback-relayed channels (换线:
//!      register channel conn → cancel the real conn token → the socket
//!      drops with the connection), close the TCP/TLS SIP listeners and the
//!      public HTTP/HTTPS listeners (stop-accept; established connections
//!      keep running on the predecessor),
//!   4. serve its live calls until drained: outbound in-dialog datagrams
//!      are POSTed to the successor's `/rolling/v1/relay_egress` (which
//!      `sendto`s them from the public socket), while the successor
//!      forwards in-dialog datagrams it cannot match locally to
//!      `/rolling/v1/relay_ingress` for injection into the predecessor's
//!      stack.
//!
//! Traffic ownership is naturally split: established TCP/WS connections and
//! their dialogs never leave the predecessor (native handling, no relay);
//! UDP always lands on whoever owns the socket. Only UDP needs the relay.
//!
//! Failure semantics: a stale successor heartbeat → plain SIGTERM drain; a
//! successor crash mid-window → its manifest goes stale and forwarding
//! retires (calls on the predecessor die with the crashed successor — the
//! same blast radius as a plain container restart; the container restart
//! policy brings the successor back and it re-binds the ports).
//!
//! Everything rides loopback HTTP on the management listener; no IPC
//! sockets, no fd passing, no permanent middleman.

use crate::app::AppState;
use crate::config::RollingConfig;
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::{Method, SipMessage};
use rsipstack::sip::transport::Transport;
use rsipstack::transport::{SipAddr, SipConnection, TransportEvent, channel::ChannelConnection};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncRead;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// URL path base for the rolling endpoints (merged into the main router).
pub const ROLLING_BASE: &str = "/rolling/v1";

const ROLE_IDLE: u8 = 0; // successor (or plain process)
const ROLE_PREDECESSOR: u8 = 1;

/// In-dialog datagram captured while the predecessor's manifest was not yet
/// known (the milliseconds between "predecessor closed its listeners" and
/// "first manifest arrived"). Held briefly, then forwarded or dropped.
struct PendingForward {
    at: Instant,
    src: SocketAddr,
    data: Vec<u8>,
}

const PENDING_CAP: usize = 64;
const PENDING_TTL: Duration = Duration::from_secs(3);

/// Shared runtime for both roles. Created by the builder when
/// `[rolling] enabled = true`; `None` otherwise (fully inert).
pub struct RollingRuntime {
    pub config: RollingConfig,
    /// Loopback port of the management listener (from `http_addr`).
    http_port: u16,
    role: AtomicU8,
    /// predecessor side: last FOREIGN successor heartbeat (unix ms).
    /// Self-pings (a lone instance probing its own loopback) are ignored —
    /// otherwise every standalone process would believe a handover pair
    /// exists and SIGTERM would attempt a handover to itself.
    last_hello_ms: AtomicI64,
    /// Random per-instance id for hello self-detection: a lone instance
    /// probing its own loopback sees ITS OWN id; a rolling pair sees the
    /// peer's (different even for two generations in one process).
    instance_id: u64,
    /// successor side: public listeners are owned (handover done).
    bind_done: AtomicBool,
    /// successor side: last manifest refresh (unix ms).
    manifest_ms: AtomicI64,
    /// successor side: call-ids the predecessor still owns (manifest).
    old_calls: std::sync::RwLock<HashSet<String>>,
    /// successor side: local UDP sockets for `relay_egress` sendto.
    udp_sockets: std::sync::Mutex<Vec<(SocketAddr, Arc<tokio::net::UdpSocket>)>>,
    /// successor side: in-dialog datagrams captured before the first
    /// manifest arrived.
    pending: std::sync::Mutex<Vec<PendingForward>>,
    /// successor side: wired by the builder once the SIP server exists (the
    /// UDP pump needs local-dialog checks).
    sip: std::sync::OnceLock<crate::proxy::server::SipServerRef>,
    /// predecessor side: channel-relay handles for the swapped UDP
    /// transports (inbound injection targets).
    udp_relays: std::sync::Mutex<Vec<UdpRelayHandle>>,
    /// predecessor side: how to close live client connections (drain-time
    /// idle eviction). Populated by `ws.rs` for bridged WebSocket sessions.
    real_closes: dashmap::DashMap<(String, SocketAddr), Arc<dyn Fn() + Send + Sync>>,
    /// Extra "do not evict connections yet" gates (e.g. the CC queue has
    /// waiters → agents must stay reachable by the draining queue).
    busy_predicates: std::sync::Mutex<Vec<Arc<dyn Fn() -> bool + Send + Sync>>>,
    /// predecessor side: loopback listener lifecycle.
    token: CancellationToken,
    /// Cancels the UDP pumps created by `create_udp_transport` so the
    /// sockets drop and the successor can bind at handover (the
    /// predecessor's UDP sockets must be released when its transports swap
    /// to the loopback relay).
    udp_close: CancellationToken,
    /// predecessor side: cancels the PUBLIC http/https accept loops without
    /// touching the established connections (app.rs owns the loops).
    close_http: CancellationToken,
    /// Shared HTTP client (loopback POSTs).
    http: reqwest::Client,
}

struct UdpRelayHandle {
    to_sip: tokio::sync::mpsc::UnboundedSender<TransportEvent>,
    conn: SipConnection,
    sip_addr: SipAddr,
}

impl RollingRuntime {
    pub fn new(config: RollingConfig, http_addr: &str) -> Arc<Self> {
        let http_port = http_addr
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8080);
        Arc::new(Self {
            config,
            http_port,
            role: AtomicU8::new(ROLE_IDLE),
            instance_id: rand::random::<u64>(),
            last_hello_ms: AtomicI64::new(0),
            bind_done: AtomicBool::new(false),
            manifest_ms: AtomicI64::new(0),
            old_calls: std::sync::RwLock::new(HashSet::new()),
            udp_sockets: std::sync::Mutex::new(Vec::new()),
            pending: std::sync::Mutex::new(Vec::new()),
            sip: std::sync::OnceLock::new(),
            udp_relays: std::sync::Mutex::new(Vec::new()),
            real_closes: dashmap::DashMap::new(),
            busy_predicates: std::sync::Mutex::new(Vec::new()),
            token: CancellationToken::new(),
            udp_close: CancellationToken::new(),
            close_http: CancellationToken::new(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("rolling http client"),
        })
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn is_predecessor(&self) -> bool {
        self.role.load(Ordering::SeqCst) == ROLE_PREDECESSOR
    }

    pub fn is_predecessor_role(&self) -> bool {
        self.is_predecessor()
    }

    /// Wire the local SIP server (called by the builder after the AppState
    /// exists — the UDP pump needs local-dialog checks).
    pub fn attach_sip_server(&self, sip: crate::proxy::server::SipServerRef) {
        let _ = self.sip.set(sip);
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// `http://127.0.0.1:<port>` — the loopback face of the management
    /// listener (host-network deployment: both containers share loopback).
    fn loopback_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.http_port)
    }

    fn predecessor_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.config.predecessor_port)
    }

    /// All rolling control POSTs MUST disable keep-alive: the shared
    /// management port is owned by WHOEVER holds the listeners, and an
    /// established pooled connection keeps being served by the
    /// predecessor's (preserved) established connections long after its
    /// listener closed — a manifest pushed over such a connection lands on
    /// the WRONG generation and the successor never learns the call-id set.
    fn post_json(
        http: &reqwest::Client,
        url: &str,
        payload: &serde_json::Value,
    ) -> reqwest::RequestBuilder {
        http.post(url)
            .json(payload)
            .header(axum::http::header::CONNECTION, "close")
    }

    /// The successor's management base URL (same config → same http port).
    /// The live UDP socket bound to `local` (outbound pump lookup — the
    /// socket is RELEASED at the handover swap, so absence means the
    /// predecessor relay path applies).
    fn socket_for(&self, local: SocketAddr) -> Option<Arc<tokio::net::UdpSocket>> {
        self.udp_sockets
            .lock()
            .unwrap()
            .iter()
            .find(|(addr, _)| *addr == local)
            .map(|(_, s)| s.clone())
    }

    // ─────────────────────────────────────────────────────────────
    // successor side
    // ─────────────────────────────────────────────────────────────

    /// Hello heartbeat: POST `/rolling/v1/hello` to the predecessor every
    /// interval until the successor owns the public listeners. Started by
    /// the builder right after the AppState is built.
    pub fn start_hello_pinger(self: &Arc<Self>) {
        if !self.enabled() {
            return;
        }
        let runtime = self.clone();
        let interval = Duration::from_secs(self.config.hello_interval_secs.max(1));
        crate::utils::spawn(async move {
            let url = format!("{}{ROLLING_BASE}/hello", runtime.loopback_base());
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if runtime.bind_done.load(Ordering::SeqCst) || runtime.is_predecessor() {
                    return;
                }
                let payload = serde_json::json!({
                    "instance_id": runtime.instance_id,
                    "pid": std::process::id(),
                    "version": env!("CARGO_PKG_VERSION"),
                });
                match RollingRuntime::post_json(&runtime.http, &url, &payload)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        debug!("rolling hello acknowledged by predecessor");
                    }
                    Ok(resp) => {
                        debug!(status = %resp.status(), "rolling hello rejected (predecessor not rolling?)");
                    }
                    Err(_) => {
                        // No predecessor running (plain start / already gone).
                        debug!("rolling hello: predecessor unreachable");
                    }
                }
            }
        });
    }

    /// Successor took over the public listeners — stop hello-pinging.
    pub fn mark_bound(&self) {
        if !self.bind_done.swap(true, Ordering::SeqCst) {
            info!("rolling: successor owns the public listeners — takeover complete");
        }
    }

    /// The token that closes the PUBLIC http/https accept loops at handover
    /// (established connections are preserved).
    pub fn close_http_token(&self) -> CancellationToken {
        self.close_http.clone()
    }

    /// Refresh the predecessor's live call-id set (manifest push) and flush
    /// datagrams buffered before the first manifest.
    pub fn note_manifest(self: &Arc<Self>, call_ids: Vec<String>) {
        {
            let mut set = self.old_calls.write().unwrap();
            set.clear();
            set.extend(call_ids.iter().cloned());
        }
        let was_stale = !self.predecessor_alive();
        let count = call_ids.len();
        self.manifest_ms.store(Self::now_ms(), Ordering::SeqCst);
        info!(
            calls = count,
            was_stale,
            pred = self.is_predecessor_role(),
            "rolling: manifest refreshed"
        );

        // Flush pending forwards (Arc-owned task; no lifetime tricks).
        let pending: Vec<PendingForward> = {
            let mut guard = self.pending.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        if pending.is_empty() {
            return;
        }
        let runtime = self.clone();
        crate::utils::spawn(async move {
            for item in pending {
                if item.at.elapsed() > PENDING_TTL {
                    continue;
                }
                runtime.forward_to_predecessor(item.src, &item.data).await;
            }
        });
    }

    /// Fresh manifest = the predecessor is alive and relaying.
    pub fn predecessor_alive(&self) -> bool {
        let last = self.manifest_ms.load(Ordering::SeqCst);
        last > 0
            && Self::now_ms() - last <= self.config.manifest_fresh_window().as_millis() as i64
    }

    fn old_call_known(&self, call_id: &str) -> bool {
        let set = self.old_calls.read().unwrap();
        if set.contains(call_id) {
            return true;
        }
        // Dual-key, mirroring the master's DialogTable: manifest entries are
        // derived from dialog ids via the bare-key rule, so a call-id that
        // itself contains '-' must also match through its own bare segment.
        wire_call_id_of(call_id)
            .map(|w| set.contains(w))
            .unwrap_or(false)
    }

    fn locally_known(&self, call_id: &str) -> bool {
        self.sip
            .get()
            .map(|sip| {
                sip.active_call_registry
                    .get_handle_by_call_id(call_id)
                    .is_some()
            })
            .unwrap_or(false)
    }

    /// Buffer an in-dialog datagram captured during the manifest gap.
    fn buffer_pending(&self, src: SocketAddr, data: &[u8]) {
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= PENDING_CAP {
            pending.remove(0);
        }
        pending.push(PendingForward {
            at: Instant::now(),
            src,
            data: data.to_vec(),
        });
    }

    /// POST the datagram to the predecessor's loopback relay listener.
    /// Returns `true` when handled (the caller must skip local injection).
    async fn forward_to_predecessor(&self, src: SocketAddr, data: &[u8]) -> bool {
        let payload = serde_json::json!({
            "src": src.to_string(),
            "data": String::from_utf8_lossy(data),
        });
        let url = format!("{}{ROLLING_BASE}/relay_ingress", self.predecessor_base());
        match RollingRuntime::post_json(&self.http, &url, &payload)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                debug!(%src, bytes = data.len(), "rolling: forwarded in-dialog datagram to predecessor");
                true
            }
            Ok(resp) => {
                debug!(status = %resp.status(), "rolling: predecessor rejected relay");
                false
            }
            Err(_) => false,
        }
    }

    /// Register a local UDP socket for the `relay_egress` sendto path.
    pub fn register_udp_socket(&self, local: SocketAddr, sock: Arc<tokio::net::UdpSocket>) {
        self.udp_sockets.lock().unwrap().push((local, sock));
    }

    /// old → new: send a datagram from the successor's public UDP socket.
    async fn relay_egress(&self, local: Option<String>, dst: String, data: String) -> Response {
        let dst: SocketAddr = match dst.parse() {
            Ok(a) => a,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"error": "bad dst"})),
                )
                    .into_response();
            }
        };
        let sock = {
            let sockets = self.udp_sockets.lock().unwrap();
            let by_port = local.as_deref().and_then(|l| l.rsplit(':').next());
            match by_port {
                Some(port) => sockets
                    .iter()
                    .find(|(addr, _)| addr.port().to_string() == port)
                    .or_else(|| sockets.first())
                    .map(|(_, s)| s.clone()),
                None => sockets.first().map(|(_, s)| s.clone()),
            }
        };
        let Some(sock) = sock else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "no udp socket"})),
            )
                .into_response();
        };
        match sock.send_to(data.as_bytes(), dst).await {
            Ok(n) => {
                debug!(%dst, sent = n, "rolling: relay_egress sent");
                StatusCode::OK.into_response()
            }
            Err(e) => {
                warn!(%dst, error = %e, "rolling: relay_egress send failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response()
            }
        }
    }

    // ─────────────────────────────────────────────────────────────
    // predecessor side
    // ─────────────────────────────────────────────────────────────

    /// hello heartbeat from the successor. Self-pings (a lone instance
    /// probing its own loopback) are IGNORED — they must never make a
    /// standalone process believe a handover pair exists.
    pub fn note_hello(&self, instance_id: u64) {
        if instance_id == self.instance_id {
            debug!("rolling hello: self-ping ignored");
            return;
        }
        self.last_hello_ms.store(Self::now_ms(), Ordering::SeqCst);
    }

    /// Fresh successor heartbeat? Gates the SIGTERM handover decision.
    pub fn successor_heartbeat_fresh(&self) -> bool {
        let last = self.last_hello_ms.load(Ordering::SeqCst);
        if last == 0 {
            return false;
        }
        let fresh = self.config.hello_fresh_window();
        Self::now_ms() - last <= fresh.as_millis() as i64
    }

    /// Register a live client connection closer for drain-time idle
    /// eviction (ws.rs bridges register themselves here).
    pub fn register_real_close(
        &self,
        kind: &str,
        peer: SocketAddr,
        close: Arc<dyn Fn() + Send + Sync>,
    ) {
        self.real_closes.insert((kind.to_string(), peer), close);
    }

    pub fn unregister_real_close(&self, kind: &str, peer: SocketAddr) {
        self.real_closes.remove(&(kind.to_string(), peer));
    }

    /// Register an extra busy gate for the eviction loop (e.g. the CC queue
    /// has waiters → agents must stay reachable from the draining queue).
    pub fn register_busy_predicate(&self, predicate: Arc<dyn Fn() -> bool + Send + Sync>) {
        self.busy_predicates.lock().unwrap().push(predicate);
    }

    /// new → old: inject a datagram into the predecessor's SIP stack as if
    /// it had arrived on the (now closed) public socket. `src` is the REAL
    /// remote address so Via received/rport stamping stays truthful.
    async fn relay_ingress(&self, src: String, data: String) -> Response {
        let src: SocketAddr = match src.parse() {
            Ok(a) => a,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"error": "bad src"})),
                )
                    .into_response();
            }
        };
        let relay = {
            let relays = self.udp_relays.lock().unwrap();
            relays.first().map(|r| UdpRelayHandle {
                to_sip: r.to_sip.clone(),
                conn: r.conn.clone(),
                sip_addr: r.sip_addr.clone(),
            })
        };
        let Some(relay) = relay else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "no relay"})),
            )
                .into_response();
        };
        match SipMessage::try_from(data.as_bytes()) {
            Ok(msg) => {
                let msg = match SipConnection::update_msg_received(msg, src, Transport::Udp) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!(error = %e, "rolling relay_ingress: via stamp failed");
                        return StatusCode::BAD_REQUEST.into_response();
                    }
                };
                let _ = relay.to_sip.send(TransportEvent::Incoming(
                    msg,
                    relay.conn.clone(),
                    relay.sip_addr.clone(),
                ));
                StatusCode::OK.into_response()
            }
            Err(e) => {
                debug!(error = %e, "rolling relay_ingress: unparsable payload");
                StatusCode::BAD_REQUEST.into_response()
            }
        }
    }

    /// The SIGTERM entry point for rolling-enabled processes. Attempts the
    /// handover; falls back to the plain drain on any refusal/failure.
    pub async fn handle_termination(self: &Arc<Self>, state: &AppState, router: Router) {
        if self.is_predecessor() {
            // Double SIGTERM during the window: join the existing drain.
            crate::builder::drain_and_stop(state).await;
            return;
        }
        if !self.successor_heartbeat_fresh() {
            info!(
                "rolling: no fresh successor heartbeat — falling back to plain drain \
                 (start the successor container first, then docker stop the predecessor)"
            );
            crate::builder::drain_and_stop(state).await;
            return;
        }
        info!("rolling: fresh successor heartbeat — starting handover");
        match self.begin_handover(state, router).await {
            Ok(()) => {
                // Drain completed (or timed out) — same exit contract as
                // drain_and_stop: cancel so app_future completes.
                state.token().cancel();
            }
            Err(e) => {
                warn!(error = %e, "rolling: handover failed — falling back to plain drain");
                crate::builder::drain_and_stop(state).await;
            }
        }
    }

    async fn begin_handover(
        self: &Arc<Self>,
        state: &AppState,
        router: Router,
    ) -> anyhow::Result<()> {
        let started = Instant::now();
        self.role.store(ROLE_PREDECESSOR, Ordering::SeqCst);
        crate::shutdown::initiate();
        info!(
            active_calls = state.sip_server().inner.active_call_registry.count(),
            "rolling: handover started (draining; live calls keep running on this process)"
        );

        // Cluster interplay: stop refreshing this node's session rows. The
        // successor (same node id, same config) owns the identity from now
        // on; rows for calls still on this process age out naturally once
        // they end here.
        if let Some(hb) = state
            .sip_server()
            .inner
            .session_registry_heartbeat
            .lock()
            .unwrap()
            .take()
        {
            drop(hb);
            info!("rolling: predecessor session-registry heartbeat stopped (successor owns node identity)");
        }

        // 1. Loopback predecessor listener FIRST — if the port is taken we
        //    abort while every public listener is still intact.
        self.serve_predecessor_listener(router).await?;

        // 2. Swap UDP transports (换线) + close the stream listeners.
        self.swap_and_close_transports(state);

        // 3. Stop accepting on the public HTTP/HTTPS listeners WITHOUT
        //    killing established connections (WS clients stay here).
        // Close the public HTTP/HTTPS accept loops (established conns kept).
        self.close_http.cancel();
        // Release the predecessor's UDP sockets: the successor binds the
        // instant these ports are free (the pumps exit via udp_close).
        self.udp_close.cancel();
        self.udp_sockets.lock().unwrap().clear();
        info!(
            "rolling: public listeners closed ({}ms)",
            started.elapsed().as_millis()
        );

        // 4. Handoff payload + manifest reporter. The successor binds within
        //    milliseconds of step 3 — retry until it answers.
        let base = self.loopback_base();
        self.push_handoff_until_accepted(state, &base).await;
        self.start_manifest_reporter(state, &base);

        // 5. Evict idle WS clients (paced) so agents re-home onto the
        //    successor; conns carrying live dialogs are skipped.
        self.start_eviction_loop(state);

        // 6. Drain: same predicate as the plain SIGTERM path.
        let sip = state.sip_server();
        let timeout = state.config().graceful_shutdown_timeout();
        let outcome = crate::shutdown::wait_until_drained(
            || {
                sip.inner.active_call_registry.count() == 0
                    && !sip.inner.dialog_layer.all_dialog_ids().iter().any(|id| {
                        matches!(
                            sip.inner.dialog_layer.get_dialog_with(id),
                            Some(rsipstack::dialog::dialog::Dialog::Invite(_))
                        )
                    })
            },
            timeout,
        )
        .await;
        crate::shutdown::log_drain_outcome(outcome);
        self.token.cancel();
        info!(
            elapsed_s = started.elapsed().as_secs(),
            "rolling: handover window complete"
        );
        Ok(())
    }

    /// Loopback listener (127.0.0.1 ONLY) serving the full management
    /// router — AMI/console ops for the predecessor's live resources are
    /// proxied here by the successor, and `/rolling/v1/relay_ingress` rides
    /// the same router.
    async fn serve_predecessor_listener(
        self: &Arc<Self>,
        router: Router,
    ) -> anyhow::Result<()> {
        let addr = SocketAddr::from(([127, 0, 0, 1], self.config.predecessor_port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!("predecessor port {} unavailable: {}", addr, e))?;
        let close = self.token.clone();
        crate::utils::spawn(async move {
            crate::rolling::run_public_http_listener(listener, router, None, close).await;
        });
        info!(
            port = self.config.predecessor_port,
            "rolling: predecessor loopback listener up (management router + relay)"
        );
        Ok(())
    }

    /// 换线 for the bound transports:
    /// * rolling UDP (ChannelConnection owned by this module): switch the
    ///   outbound pump to relay mode via the role flag, kill the
    ///   real-socket read pump and RELEASE the socket — the successor binds
    ///   the instant the port is free. The channel itself stays registered:
    ///   dialogs keep their affinity, and injected ingress + relayed egress
    ///   ride the same connection.
    /// * stream listeners (TCP/TLS/WS): cancelled outright — established
    ///   stream connections live in their own tasks and keep flowing.
    fn swap_and_close_transports(&self, state: &AppState) {
        let sip = state.sip_server();
        let endpoint = sip.inner.endpoint.inner.clone();
        let transports: Vec<SipConnection> = {
            let mut guard = sip.inner.bound_transports.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for conn in transports {
            let sip_addr = conn.get_addr().clone();
            if sip_addr.r#type == Some(Transport::Udp) {
                // Rolling pump transport (ChannelConnection owned by this
                // module): keep the channel registered — dialogs keep their
                // affinity and responses ride it — flip its outbound pump to
                // relay mode (role flag), kill the real-socket read pump and
                // RELEASE the socket (the port frees for the successor).
                self.udp_close.cancel();
                self.udp_sockets.lock().unwrap().clear();
                info!(%sip_addr, "rolling: UDP transport switched to loopback relay (channel preserved)");
                continue;
            }
            // Stream listeners (TCP/TLS/WS): cancelled outright —
            // established stream connections live in their own tasks and
            // keep flowing on this process until their dialogs end.
            info!(%sip_addr, "rolling: stream listener closed (established conns preserved)");
            if let Some(token) = conn.cancel_token() {
                token.cancel();
            }
            endpoint.transport_layer.del_transport(&sip_addr);
            endpoint.transport_layer.remove_connection(&conn);
        }
    }

    /// Handoff payload: live call-id manifest (+ optional registration
    /// dump). Retries until the successor answers or `bind_wait_secs`
    /// expires.
    async fn push_handoff_until_accepted(&self, state: &AppState, successor_base: &str) {
        let deadline = Instant::now() + Duration::from_secs(self.config.bind_wait_secs.max(5));
        let sip = state.sip_server();
        loop {
            let call_ids: Vec<String> = live_call_ids(&sip.inner);
            let url = format!("{successor_base}{ROLLING_BASE}/manifest");
            let payload = serde_json::json!({ "call_ids": call_ids });
            let (ok, successor_answered) =
                match RollingRuntime::post_json(&self.http, &url, &payload)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status_ok = resp.status().is_success();
                        // The role marker tells a self-echo (served by THIS
                        // process's router during the close race) apart from
                        // the real successor takeover.
                        let role = resp
                            .json::<serde_json::Value>()
                            .await
                            .ok()
                            .and_then(|v| {
                                v.get("role").and_then(|r| r.as_str()).map(str::to_string)
                            });
                        (
                            status_ok && role.as_deref() == Some("successor"),
                            status_ok,
                        )
                    }
                    Err(_) => (false, false),
                };

            if ok && self.config.handoff_registrations {
                self.push_registrations(state, successor_base).await;
            }
            if ok {
                info!(
                    calls = call_ids.len(),
                    "rolling: handoff manifest accepted by successor"
                );
                return;
            }
            if successor_answered {
                // A predecessor-role 200 (self-echo during the close race):
                // keep retrying until the real successor answers.
                tokio::time::sleep(Duration::from_millis(200)).await;
                if Instant::now() >= deadline {
                    warn!("rolling: successor did not accept the handoff manifest in time");
                    return;
                }
                continue;
            }
            if Instant::now() >= deadline {
                warn!("rolling: successor did not accept the handoff manifest in time");
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn push_registrations(&self, state: &AppState, successor_base: &str) {
        let dump = state.sip_server().inner.locator.dump_registrations().await;
        if dump.is_empty() {
            return;
        }
        let entries: Vec<serde_json::Value> = dump
            .iter()
            .map(|(aor, loc)| {
                serde_json::json!({
                    "aor": aor,
                    "destination": loc.destination.as_ref().map(|d| d.to_string()),
                    "expires": loc.expires,
                })
            })
            .collect();
        let url = format!("{successor_base}{ROLLING_BASE}/handoff_registrations");
        let ok = RollingRuntime::post_json(
            &self.http,
            &url,
            &serde_json::json!({ "registrations": entries }),
        )
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            info!(count = entries.len(), "rolling: registrations handed off");
        } else {
            warn!("rolling: registration handoff failed (successor re-learns via re-REGISTER)");
        }
    }

    /// Push the live call-id set every 2s for the whole window. Calls only
    /// END on this process, so the set shrinks; the successor prunes and
    /// stops forwarding for ended calls.
    fn start_manifest_reporter(self: &Arc<Self>, state: &AppState, successor_base: &str) {
        let sip = state.sip_server().inner.clone();
        let url = format!("{successor_base}{ROLLING_BASE}/manifest");
        let close = self.token.clone();
        let http = self.http.clone();
        crate::utils::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = close.cancelled() => return,
                    _ = ticker.tick() => {
                        let call_ids = live_call_ids(&sip);
                        let payload = serde_json::json!({ "call_ids": call_ids });
                        let _ = RollingRuntime::post_json(&http, &url, &payload)
                            .timeout(Duration::from_secs(2))
                            .send()
                            .await;
                    }
                }
            }
        });
    }

    /// Paced idle-connection eviction: WS clients without live dialogs are
    /// disconnected so they reconnect onto the successor. Gated by the busy
    /// predicates (queue has waiters → keep agents here until it drains).
    /// `pub` for the rolling e2e (the gate test drives it directly).
    pub fn start_eviction_loop(self: &Arc<Self>, state: &AppState) {
        let runtime = self.clone();
        let sip = state.sip_server().inner.clone();
        let close = self.token.clone();
        crate::utils::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(2));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = close.cancelled() => return,
                    _ = ticker.tick() => {
                        // Busy gates: explicit predicates (e.g. CC skill-group
                        // queue state) plus the registry's own queue marker —
                        // while ANY caller waits in a queue, agents must stay
                        // reachable from this process (evicting them strands
                        // the waiters: the successor cannot reach this
                        // generation's queue calls).
                        let gate_busy = runtime
                            .busy_predicates
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|p| p())
                            || sip.active_call_registry.has_queued_calls();
                        if gate_busy {
                            debug!("rolling: eviction paused — queue has waiters (busy gate active)");
                            continue;
                        }
                        let busy: HashSet<(String, u16)> = sip
                            .dialog_layer
                            .all_dialog_ids()
                            .iter()
                            .filter_map(|id| {
                                let dialog = sip.dialog_layer.get_dialog_with(id)?;
                                let contact = dialog.remote_contact()?;
                                Some((
                                    contact.host().to_string(),
                                    contact.host_with_port.port.map(|p| p.0).unwrap_or(0),
                                ))
                            })
                            .collect();
                        // Paced eviction: at most EVICT_BATCH_CAP connections
                        // per round, each with a small random gap — a mass
                        // disconnect would thundering-herd the successor with
                        // simultaneous TLS handshakes + REGISTERs.
                        let evictable: Vec<(String, SocketAddr)> = runtime
                            .real_closes
                            .iter()
                            .map(|e| e.key().clone())
                            .filter(|key| !busy.contains(&(key.1.ip().to_string(), key.1.port())))
                            .collect();
                        let batch = select_evict_batch(&evictable, EVICT_BATCH_CAP);
                        let mut evicted = 0usize;
                        for key in batch {
                            if let Some((_, close_fn)) = runtime.real_closes.remove(&key) {
                                debug!(peer = %key.1, kind = %key.0, "rolling: evicting idle connection");
                                close_fn();
                                evicted += 1;
                                // Spread the disconnects inside the round.
                                tokio::time::sleep(Duration::from_millis(evict_gap_ms())).await;
                            }
                        }
                        if evicted > 0 {
                            info!(evicted, "rolling: idle connections evicted");
                        }
                    }
                }
            }
        });
    }
}

/// Live wire Call-IDs of the local dialog layer (bare keys — the same
/// derivation the master's DialogTable used).
/// Live wire Call-IDs of the local dialog layer. Both keys are pushed
/// (full dialog id + bare wire Call-ID) so the successor's membership check
/// matches regardless of whether the call-id itself contains '-'.
fn live_call_ids(sip: &crate::proxy::server::SipServerRef) -> Vec<String> {
    let mut out = Vec::new();
    for id in sip.dialog_layer.all_dialog_ids() {
        out.push(id.clone());
        if let Some(wire) = wire_call_id_of(&id) {
            out.push(wire.to_string());
        }
    }
    out
}

/// Bare wire Call-ID of a rsipstack DialogId (`{call_id}-{local}-{remote}`).
fn wire_call_id_of(dialog_id: &str) -> Option<&str> {
    dialog_id.split_once('-').map(|(c, _)| c)
}

/// Paced eviction knobs: batch cap per 2s round and the per-disconnect
/// jitter base. Keeps a mass agent re-homing from thundering-herding the
/// successor with simultaneous handshakes + REGISTERs.
const EVICT_BATCH_CAP: usize = 20;
const EVICT_JITTER_MS: u64 = 50;

/// Pick the next eviction batch: idle keys only, capped. Order follows the
/// registry snapshot; keys beyond the cap are picked up by the next round.
fn select_evict_batch(evictable: &[(String, SocketAddr)], cap: usize) -> Vec<(String, SocketAddr)> {
    evictable.iter().take(cap.max(1)).cloned().collect()
}

/// Per-disconnect gap: jittered sleep inside the round.
fn evict_gap_ms() -> u64 {
    EVICT_JITTER_MS + rand::random::<u64>() % 150
}

/// Response destination per RFC 3581 (Via received/rport preferred). Same
/// rule as the worker UDP relay.
fn response_dest_from_via(raw: &str) -> Option<SocketAddr> {
    let via = raw
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("via:"))?;
    let rest = via.split_once(':')?.1.trim();
    let hp = rest.split(';').next()?.trim();
    let hp = hp
        .strip_prefix("SIP/2.0/UDP ")
        .or_else(|| hp.strip_prefix("SIP/2.0/TCP "))
        .or_else(|| hp.strip_prefix("SIP/2.0/TLS "))
        .or_else(|| hp.strip_prefix("SIP/2.0/WSS "))
        .unwrap_or(hp);
    let sent_by: SocketAddr = hp.parse().ok()?;

    let mut received: Option<std::net::IpAddr> = None;
    let mut rport: Option<u16> = None;
    for param in rest.split(';').skip(1) {
        let param = param.trim();
        if let Some(v) = param.strip_prefix("received=") {
            received = v.trim().parse().ok();
        } else if let Some(v) = param.strip_prefix("rport=") {
            rport = v.trim().parse().ok();
        }
    }
    match received {
        Some(ip) => Some(SocketAddr::new(ip, rport.unwrap_or(sent_by.port()))),
        None => rport
            .map(|p| SocketAddr::new(sent_by.ip(), p))
            .or(Some(sent_by)),
    }
}

fn has_to_tag(msg: &SipMessage) -> bool {
    // rsipstack's To::tag() is `Result<Option<Tag>>` — Ok(None) means the
    // To header carries NO tag (out-of-dialog request).
    msg.to_header()
        .map(|t| matches!(t.tag(), Ok(Some(_))))
        .unwrap_or(false)
}

// ── successor-side UDP transport ────────────────────────────────────────────

/// UDP transport used by the SUCCESSOR while rolling is enabled: a real
/// socket owned by this module so the read pump makes per-datagram forward
/// decisions BEFORE any transaction is created (pre-transaction hook).
pub async fn create_udp_transport(
    local_addr: SocketAddr,
    rolling: &Arc<RollingRuntime>,
) -> anyhow::Result<SipConnection> {
    let std_sock = std::net::UdpSocket::bind(local_addr)
        .map_err(|e| anyhow::anyhow!("Failed to bind UDP {}: {}", local_addr, e))?;
    std_sock.set_nonblocking(true)?;
    let sock = Arc::new(tokio::net::UdpSocket::from_std(std_sock)?);
    rolling.register_udp_socket(local_addr, sock.clone());

    let sip_addr = SipAddr {
        r#type: Some(Transport::Udp),
        addr: local_addr.into(),
    };
    // to_sip channel: rolling pump injects inbound datagrams into the stack.
    // from_sip channel: the stack hands us outbound messages for sendto.
    let (to_sip_tx, to_sip_rx) = tokio::sync::mpsc::unbounded_channel();
    let (from_sip_tx, mut from_sip_rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = ChannelConnection::create_connection(to_sip_rx, from_sip_tx, sip_addr.clone(), None)
        .await
        .map_err(|e| anyhow::anyhow!("rolling udp: channel conn failed: {}", e))?;
    let sip_conn = SipConnection::Channel(conn.clone());

    // Relay-ingress injection target: relay_ingress (from the successor)
    // injects into THIS channel — the same connection the dialogs bind to,
    // so responses ride the dialog-affinity outbound pump below.
    rolling.udp_relays.lock().unwrap().push(UdpRelayHandle {
        to_sip: to_sip_tx.clone(),
        conn: sip_conn.clone(),
        sip_addr: sip_addr.clone(),
    });

    // Outbound: endpoint → sendto (successor / idle role) or POST to the
    // successor's relay_egress (predecessor role, after the handover swap).
    // The pump NEVER exits: it is the dialog-affinity outbound path for the
    // whole process lifetime; only its backend switches at the handover.
    // The socket handle is looked up per send so the swap can release the
    // real socket (freeing the port for the successor) without killing the
    // pump.
    let local_str = local_addr.to_string();
    let pump_runtime = rolling.clone();
    tokio::spawn(async move {
        info!(%local_str, "rolling probe: primary outbound pump started");
        loop {
            tokio::select! {
                event = from_sip_rx.recv() => {
                    let Some(event) = event else {
                        warn!(%local_str, "rolling probe: outbound pump EXITED (channel closed)");
                        return;
                    };
                    let TransportEvent::Incoming(ref msg, _, _) = event else {
                        continue;
                    };
                    debug!(pred = pump_runtime.is_predecessor_role(), "rolling: primary-pump event");
            let raw = msg.to_string();
            let dst = match msg {
                SipMessage::Request(req) => {
                    let hwp = &req.uri.host_with_port;
                    format!(
                        "{}:{}",
                        hwp.host,
                        hwp.port
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "5060".into())
                    )
                    .parse::<SocketAddr>()
                    .ok()
                }
                _ => response_dest_from_via(&raw),
            };
            let Some(dst) = dst else { continue };
            if pump_runtime.is_predecessor_role() {
                let payload = serde_json::json!({
                    "local": local_str,
                    "dst": dst.to_string(),
                    "data": raw,
                });
                let url = format!("{}{ROLLING_BASE}/relay_egress", pump_runtime.loopback_base());
                // The successor rebinds within milliseconds of the listener
                // close — retry once before dropping (requests are
                // retransmitted by the transaction layer; a dropped early
                // response is the accepted sub-second edge).
                for attempt in 0..2u32 {
                    match pump_runtime
                        .http
                        .post(&url)
                        .json(&payload)
                        .timeout(Duration::from_secs(3))
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => break,
                        _ if attempt == 0 => {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                        }
                        _ => {
                            warn!(%dst, "rolling: egress POST failed after retry — datagram dropped");
                        }
                    }
                }
            } else if let Some(sock) = pump_runtime.socket_for(local_addr) {
                if let Err(e) = sock.send_to(raw.as_bytes(), dst).await {
                    debug!(%dst, error = %e, "rolling udp: send_to failed");
                }
            } else {
                warn!(%dst, "rolling probe: outbound event DROPPED (no socket, not predecessor)");
            }
                }
            }
        }
    });

    // Inbound: recv_from → forward decision → stamp → inject. The pump
    // exits when `udp_close` fires (handover: the predecessor releases the
    // socket so the successor can bind).
    let pump_runtime = rolling.clone();
    let pump_sock = sock;
    let pump_conn = conn;
    let pump_addr = sip_addr;
    let pump_to_sip = to_sip_tx;
    let pump_close = rolling.udp_close.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            tokio::select! {
                _ = pump_close.cancelled() => {
                    debug!("rolling udp: pump closing (handover)");
                    return;
                }
                res = pump_sock.recv_from(&mut buf) => {
                    match res {
                        Ok((n, src)) => {
                            let data = &buf[..n];
                            let relayed = pump_runtime.pump_inbound(src, data).await;
                            if relayed {
                                continue;
                            }
                            match SipMessage::try_from(data) {
                                Ok(msg) => {
                                    let msg = match SipConnection::update_msg_received(
                                        msg, src, Transport::Udp,
                                    ) {
                                        Ok(m) => m,
                                        Err(_) => continue,
                                    };
                                    let _ = pump_to_sip.send(TransportEvent::Incoming(
                                        msg,
                                        SipConnection::Channel(pump_conn.clone()),
                                        pump_addr.clone(),
                                    ));
                                }
                                Err(_) => {}
                            }
                        }
                        Err(e) => {
                            debug!(error = %e, "rolling udp: recv failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        }
    });

    Ok(sip_conn)
}

impl RollingRuntime {
    /// Successor-side UDP read-pump decision for one datagram. Returns
    /// `true` when handled by the relay path (caller must skip local
    /// injection).
    async fn pump_inbound(self: &Arc<Self>, src: SocketAddr, data: &[u8]) -> bool {
        let Ok(msg) = SipMessage::try_from(data) else {
            return false;
        };
        let Some(call_id) = msg.call_id_header().ok().map(|c| c.value().to_string()) else {
            return false;
        };
        let known_locally = self.locally_known(&call_id);

        if let SipMessage::Request(req) = &msg {
            let to_tag = has_to_tag(&msg);
            let old_owned = self.old_call_known(&call_id);
            let in_dialog_for_predecessor =
                (to_tag || (req.method == Method::Cancel && old_owned)) && !known_locally;
            if in_dialog_for_predecessor {
                if self.predecessor_alive() {
                    // Spawned: a slow relay POST must not stall the pump.
                    let runtime = self.clone();
                    let payload = data.to_vec();
                    crate::utils::spawn(async move {
                        runtime.forward_to_predecessor(src, &payload).await;
                    });
                    return true;
                }
                // Manifest gap right after the handover — buffer briefly.
                self.buffer_pending(src, data);
                return true;
            }
            return false;
        }

        // Response: forward only when the predecessor owns the call (strict
        // manifest membership — never steals the successor's own
        // transaction responses such as trunk keepalive OPTIONS). Handled
        // regardless of the POST outcome: a predecessor-owned response must
        // NEVER be injected locally; on relay failure it is dropped and the
        // predecessor's server transaction retransmits it.
        if msg.is_response() && self.old_call_known(&call_id) {
            if self.predecessor_alive() {
                let _ = self.forward_to_predecessor(src, data).await;
            }
            return true;
        }
        false
    }
}

// ── loopback/public HTTP serving (accept-loop + per-conn hyper) ─────────────

type BoxedStream = Box<dyn StreamDuplex>;

/// Combined read+write trait for the boxed per-connection stream (plain TCP
/// or TLS-wrapped).
trait StreamDuplex: AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + tokio::io::AsyncWrite + Unpin + Send> StreamDuplex for T {}

/// Serve one accepted TCP connection as HTTP/1.1 with WebSocket upgrade
/// support. `tls` wraps the stream first when provided.
pub(crate) fn serve_http_conn(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    router: Router,
    tls: Option<Arc<tokio_rustls::TlsAcceptor>>,
) {
    tokio::spawn(async move {
        let stream: BoxedStream = match tls {
            Some(acceptor) => match acceptor.accept(stream).await {
                Ok(s) => Box::new(s),
                Err(e) => {
                    debug!(error = %e, %peer, "rolling: tls accept failed");
                    return;
                }
            },
            None => Box::new(stream),
        };
        let svc = hyper_util::service::TowerToHyperService::new(RollingPeerService {
            inner: router,
            peer,
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
            .with_upgrades()
            .await;
    });
}

struct RollingPeerService {
    inner: Router,
    peer: SocketAddr,
}

impl Clone for RollingPeerService {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            peer: self.peer,
        }
    }
}

impl tower::Service<axum::http::Request<hyper::body::Incoming>> for RollingPeerService {
    type Response =
        <Router as tower::Service<axum::http::Request<hyper::body::Incoming>>>::Response;
    type Error = <Router as tower::Service<axum::http::Request<hyper::body::Incoming>>>::Error;
    type Future = <Router as tower::Service<axum::http::Request<hyper::body::Incoming>>>::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        tower::Service::<axum::http::Request<hyper::body::Incoming>>::poll_ready(
            &mut self.inner,
            cx,
        )
    }

    fn call(&mut self, mut req: axum::http::Request<hyper::body::Incoming>) -> Self::Future {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(self.peer));
        tower::Service::<axum::http::Request<hyper::body::Incoming>>::call(&mut self.inner, req)
    }
}

/// Manual accept loop for an HTTP(S) listener with a CLOSE seam: cancelling
/// `close` drops the listener (port released — the successor binds
/// immediately) while already-established connections keep running in
/// their own tasks.
pub(crate) async fn run_public_http_listener(
    listener: tokio::net::TcpListener,
    router: Router,
    tls: Option<Arc<tokio_rustls::TlsAcceptor>>,
    close: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = close.cancelled() => {
                info!("rolling: public listener closed (established connections preserved)");
                return;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        serve_http_conn(stream, peer, router.clone(), tls.clone());
                    }
                    Err(e) => {
                        debug!(error = %e, "public listener accept error");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
}

/// Build a rustls acceptor from the configured cert/key paths.
pub(crate) fn tls_acceptor_from_config(state: &AppState) -> Option<Arc<tokio_rustls::TlsAcceptor>> {
    let config = state.config();
    let (cert, key) = (&config.ssl_certificate, &config.ssl_private_key);
    let (cert, key) = (cert.as_deref()?, key.as_deref()?);
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(cert)
        .ok()?
        .flatten()
        .collect();
    let key = PrivateKeyDer::from_pem_file(key).ok()?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .ok()?;
    cfg.alpn_protocols = vec![];
    Some(Arc::new(tokio_rustls::TlsAcceptor::from(Arc::new(cfg))))
}

// ── axum routes ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
struct ManifestPayload {
    #[serde(default)]
    call_ids: Vec<String>,
}

#[derive(serde::Deserialize)]
struct EgressPayload {
    #[serde(default)]
    local: Option<String>,
    dst: String,
    data: String,
}

#[derive(serde::Deserialize)]
struct IngressPayload {
    src: String,
    data: String,
}

#[derive(serde::Deserialize)]
struct HandoffRegistrationsPayload {
    #[serde(default)]
    registrations: Vec<serde_json::Value>,
}

async fn read_json<T: serde::de::DeserializeOwned>(body: axum::body::Body) -> Option<T> {
    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(serde::Deserialize, Default)]
struct HelloPayload {
    #[serde(default)]
    instance_id: u64,
    #[serde(default)]
    pid: u32,
}

async fn hello_handler(State(state): State<AppState>, req: Request) -> Response {
    let payload = read_json::<HelloPayload>(req.into_body())
        .await
        .unwrap_or_default();
    let own = state
        .rolling
        .as_ref()
        .map(|r| r.instance_id)
        .unwrap_or_default();
    if let Some(rolling) = &state.rolling {
        rolling.note_hello(payload.instance_id);
    }
    axum::Json(serde_json::json!({ "instance_id": own, "pid": std::process::id() }))
        .into_response()
}

async fn manifest_handler(State(state): State<AppState>, req: Request) -> Response {
    if !loopback_only(&req) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(rolling) = state.rolling.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let payload = read_json::<ManifestPayload>(req.into_body())
        .await
        .unwrap_or_default();
    rolling.note_manifest(payload.call_ids);
    // Role marker: the predecessor's push loop only accepts a SUCCESSOR-role
    // acknowledgement — during the listener-close race a POST can land on
    // the predecessor's own (still-closing) router, and accepting that
    // self-echo would make the handoff believe the successor took over.
    let role = if rolling.is_predecessor_role() {
        "predecessor"
    } else {
        "successor"
    };
    axum::Json(serde_json::json!({ "accepted": true, "role": role })).into_response()
}

async fn relay_egress_handler(State(state): State<AppState>, req: Request) -> Response {
    if !loopback_only(&req) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(rolling) = state.rolling.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(payload) = read_json::<EgressPayload>(req.into_body()).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    rolling
        .relay_egress(payload.local, payload.dst, payload.data)
        .await
}

async fn relay_ingress_handler(State(state): State<AppState>, req: Request) -> Response {
    if !loopback_only(&req) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(rolling) = state.rolling.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(payload) = read_json::<IngressPayload>(req.into_body()).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    rolling.relay_ingress(payload.src, payload.data).await
}

async fn handoff_registrations_handler(State(state): State<AppState>, req: Request) -> Response {
    if !loopback_only(&req) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(payload) = read_json::<HandoffRegistrationsPayload>(req.into_body()).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut applied = 0usize;
    for entry in &payload.registrations {
        // `aor` is the locator identifier (`user@realm`), not a SIP URI.
        let Some(aor) = entry.get("aor").and_then(|v| v.as_str()) else {
            continue;
        };
        let (username, realm) = match aor.split_once('@') {
            Some((u, r)) => (u.to_string(), Some(r.to_string())),
            None => (aor.to_string(), None),
        };
        let Ok(uri) = format!("sip:{aor}").parse::<rsipstack::sip::Uri>() else {
            continue;
        };
        let destination = entry
            .get("destination")
            .and_then(|v| v.as_str())
            .and_then(|d| {
                d.rsplit_once(':').and_then(|(host, port)| {
                    let host = host.trim_matches(|c| c == '[' || c == ']');
                    let ip: std::net::IpAddr = host.parse().ok()?;
                    let port: u16 = port.parse().ok()?;
                    Some(SipAddr {
                        r#type: Some(Transport::Udp),
                        addr: SocketAddr::new(ip, port).into(),
                    })
                })
            });
        let mut location = crate::call::Location {
            aor: uri,
            expires: entry.get("expires").and_then(|v| v.as_u64()).unwrap_or(300) as u32,
            destination,
            ..Default::default()
        };
        location.contact_raw = Some(aor.to_string());
        if state
            .sip_server()
            .inner
            .locator
            .register(&username, realm.as_deref(), location)
            .await
            .is_ok()
        {
            applied += 1;
        }
    }
    info!(applied, "rolling: registrations preloaded from predecessor");
    StatusCode::OK.into_response()
}

fn loopback_only(req: &Request) -> bool {
    req.extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false)
}

/// Rolling endpoints merged into the main router (and therefore served
/// again by the predecessor loopback listener, which mounts the same
/// router).
pub fn rolling_routes(state: &AppState) -> Router {
    Router::new()
        .route(&format!("{ROLLING_BASE}/hello"), post(hello_handler))
        .route(&format!("{ROLLING_BASE}/manifest"), post(manifest_handler))
        .route(
            &format!("{ROLLING_BASE}/relay_egress"),
            post(relay_egress_handler),
        )
        .route(
            &format!("{ROLLING_BASE}/relay_ingress"),
            post(relay_ingress_handler),
        )
        .route(
            &format!("{ROLLING_BASE}/handoff_registrations"),
            post(handoff_registrations_handler),
        )
        .with_state(state.clone())
}

// ── AMI forwarding (successor → predecessor) ────────────────────────────────

/// Endpoint families whose requests target LIVE resources: when the local
/// process does not know the target and the predecessor is still draining,
/// the request is proxied to the predecessor's loopback management
/// listener.
const FORWARD_PATH_PREFIXES: &[&str] = &[
    "/hangup/",
    "/cluster/forward_sip",
    "/cluster/session_op",
    "/cluster/set_userdata",
    "/cluster/get_userdata",
    "/cluster/set_var",
    "/cluster/get_var",
    "/cluster/show_session/",
    "/cluster/session_owner/",
    "/cluster/dispatch_command",
];

fn forward_candidate_path(path: &str) -> bool {
    FORWARD_PATH_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// Extract candidate resource ids from the path and JSON body for the
/// local-liveness check.
fn extract_target_ids(path: &str, body: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = path
        .split('/')
        .filter(|seg| seg.len() >= 6)
        .map(str::to_string)
        .collect();
    for key in ["call_id", "session_id", "id"] {
        if let Some(v) = body.get(key).and_then(|v| v.as_str()) {
            ids.push(v.to_string());
        }
    }
    ids
}

/// AMI middleware (runs INSIDE the auth layer): proxies live-resource
/// requests the local process cannot satisfy to the predecessor's loopback
/// management listener.
pub async fn ami_forward_middleware(
    State(state): State<AppState>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(rolling) = state.rolling.clone() else {
        return next.run(req).await;
    };
    if !rolling.enabled()
        || rolling.is_predecessor_role()
        || !rolling.predecessor_alive()
        || !rolling.bind_done.load(Ordering::SeqCst)
    {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    let ami_path = state
        .config()
        .proxy
        .ami_path
        .clone()
        .unwrap_or_else(|| crate::config::DEFAULT_AMI_PATH.to_string());
    let inner_path = path.strip_prefix(&ami_path).unwrap_or(&path).to_string();
    if !forward_candidate_path(&inner_path) {
        return next.run(req).await;
    }

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                axum::Json(serde_json::json!({"error": "body too large to forward"})),
            )
                .into_response();
        }
    };
    let body_json: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    let sip = state.sip_server();
    let ids = extract_target_ids(&inner_path, &body_json);
    let known = !ids.is_empty()
        && ids.iter().any(|id| {
            sip.inner.dialog_layer.get_dialog_with(id).is_some()
                || sip
                    .inner
                    .active_call_registry
                    .get_handle_by_call_id(id)
                    .is_some()
        });
    if known {
        let req = Request::from_parts(parts, axum::body::Body::from(bytes));
        return next.run(req).await;
    }

    // Forward to the predecessor's loopback management listener. The
    // middleware sees the nest-STRIPPED path (`/hangup/...`) but the
    // predecessor mounts the same router with AMI nested at `ami_path` —
    // rebuild the FULL path.
    let ami_prefix = format!("/{}", ami_path.trim_matches('/'));
    let url = format!(
        "{}{}{}",
        rolling.predecessor_base(),
        ami_prefix,
        inner_path
    );
    let method = parts.method.clone();
    let mut headers = parts.headers.clone();
    headers.remove(axum::http::header::HOST);
    let forwarded = rolling
        .http
        .request(method, &url)
        .headers(headers)
        .body(bytes.to_vec())
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    match forwarded {
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = resp
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .cloned();
            let resp_body = resp.bytes().await.unwrap_or_default();
            let mut out = axum::http::Response::builder().status(status);
            if let Some(ct) = content_type {
                out = out.header(axum::http::header::CONTENT_TYPE, ct);
            }
            match out.body(axum::body::Body::from(resp_body)) {
                Ok(r) => r.into_response(),
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        Err(e) => {
            debug!(error = %e, "rolling: AMI forward to predecessor failed — serving locally");
            let req = Request::from_parts(parts, axum::body::Body::from(bytes));
            next.run(req).await
        }
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runtime() -> Arc<RollingRuntime> {
        RollingRuntime::new(RollingConfig::default(), "0.0.0.0:18081")
    }

    #[test]
    fn test_wire_call_id_extraction() {
        assert_eq!(
            wire_call_id_of("abc123-d4e5-f6a7"),
            Some("abc123"),
            "DialogId (call-local-remote): bare key is everything before the first '-'"
        );
        // A DialogId ALWAYS carries tags, so an empty id is the only None case.
        assert_eq!(wire_call_id_of(""), None);
        // Call-ids containing '-' keep their full form alongside the bare key
        // (dual-key membership in `old_call_known`).
        assert_eq!(wire_call_id_of("ab-cd-ef-12-34"), Some("ab"));
    }

    #[test]
    fn test_response_dest_from_via_received_rport() {
        let raw = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;received=192.168.3.9;rport=45908\r\nFrom: <sip:a@x>\r\nTo: <sip:b@y>\r\nCall-ID: c1\r\nCSeq: 1 BYE\r\n\r\n";
        let dst = response_dest_from_via(raw).expect("dest");
        assert_eq!(dst.to_string(), "192.168.3.9:45908");
    }

    #[test]
    fn test_response_dest_from_via_falls_back_to_sent_by() {
        let raw = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.9:5060\r\nFrom: <sip:a@x>\r\nTo: <sip:b@y>\r\nCall-ID: c1\r\nCSeq: 1 BYE\r\n\r\n";
        assert_eq!(
            response_dest_from_via(raw).expect("dest").to_string(),
            "10.0.0.9:5060"
        );
    }

    #[test]
    fn test_select_evict_batch_cap() {
        let keys: Vec<(String, SocketAddr)> = (0..50)
            .map(|i| {
                (
                    "Ws".to_string(),
                    SocketAddr::from(([10u8, 0, 0, i as u8], 5060)),
                )
            })
            .collect();
        let batch = select_evict_batch(&keys, EVICT_BATCH_CAP);
        assert_eq!(batch.len(), EVICT_BATCH_CAP, "cap limits the round");
        // Keys beyond the cap stay for subsequent rounds (20 → 20 → 10).
        let rest = select_evict_batch(&keys[EVICT_BATCH_CAP..], EVICT_BATCH_CAP);
        assert_eq!(rest.len(), EVICT_BATCH_CAP);
        let tail = select_evict_batch(&keys[2 * EVICT_BATCH_CAP..], EVICT_BATCH_CAP);
        assert_eq!(tail.len(), keys.len() - 2 * EVICT_BATCH_CAP);
        assert_eq!(select_evict_batch(&[], EVICT_BATCH_CAP).len(), 0);
    }

    #[test]
    fn test_manifest_freshness_and_heartbeat() {
        let rt = test_runtime();
        assert!(!rt.predecessor_alive(), "no manifest yet → not alive");
        rt.note_hello(0xDEAD_BEEF);
        assert!(rt.successor_heartbeat_fresh());
        rt.note_manifest(vec!["call-1".into(), "call-2".into()]);
        assert!(rt.predecessor_alive());
        assert!(rt.old_call_known("call-1"));
        assert!(!rt.old_call_known("call-3"));
        rt.note_manifest(vec!["call-2".into()]);
        assert!(!rt.old_call_known("call-1"), "ended calls leave the manifest");
        assert!(rt.old_call_known("call-2"));
    }

    #[test]
    fn test_forward_candidates() {
        assert!(forward_candidate_path("/hangup/abc-123"));
        assert!(forward_candidate_path("/cluster/forward_sip"));
        assert!(forward_candidate_path("/cluster/session_op"));
        assert!(!forward_candidate_path("/reload/trunks"));
        assert!(!forward_candidate_path("/shutdown"));
        assert!(!forward_candidate_path("/health"));
    }

    #[test]
    fn test_extract_target_ids() {
        let ids = extract_target_ids(
            "/hangup/call-abc-1-2",
            &serde_json::json!({"call_id": "xyz"}),
        );
        assert!(ids.contains(&"call-abc-1-2".to_string()));
        assert!(ids.contains(&"xyz".to_string()));
    }

    #[test]
    fn test_busy_predicate_gates_and_registry() {
        let rt = test_runtime();
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let gate_for_fn = gate.clone();
        rt.register_busy_predicate(Arc::new(move || gate_for_fn.load(Ordering::SeqCst)));

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let closer: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.send(());
        });
        let peer: SocketAddr = "10.1.1.1:5060".parse().unwrap();
        rt.register_real_close("Wss", peer, closer);
        assert_eq!(rt.real_closes.len(), 1);

        let active = || {
            rt.busy_predicates
                .lock()
                .unwrap()
                .iter()
                .any(|p| p())
        };
        assert!(active(), "queue gate reports busy");
        gate.store(false, Ordering::SeqCst);
        assert!(!active(), "gate released");

        // Eviction removes + invokes the closer (loop body semantics).
        let key = ("Wss".to_string(), peer);
        if let Some((_, close)) = rt.real_closes.remove(&key) {
            close();
        }
        assert_eq!(rt.real_closes.len(), 0);
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_pump_inbound_decisions() {
        let rt = test_runtime();
        // Manifest with one predecessor-owned call.
        rt.note_manifest(vec!["oldcall-1-2".into()]);

        let bye_for_old = "BYE sip:b@x SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK1\r\nFrom: <sip:a@x>;tag=f1\r\nTo: <sip:b@y>;tag=t1\r\nCall-ID: oldcall-1-2\r\nCSeq: 2 BYE\r\n\r\n";
        // Predecessor alive: forwarded (skips local injection).
        assert!(
            rt.pump_inbound("10.0.0.9:5060".parse().unwrap(), bye_for_old.as_bytes())
                .await
        );

        // After the manifest goes stale: buffered (still "handled", not
        // injected), flushed once a fresh manifest arrives.
        rt.manifest_ms
            .store(RollingRuntime::now_ms() - 60_000, Ordering::SeqCst);
        assert!(
            rt.pump_inbound("10.0.0.9:5060".parse().unwrap(), bye_for_old.as_bytes())
                .await
        );
        assert_eq!(rt.pending.lock().unwrap().len(), 1);
        rt.note_manifest(vec!["oldcall-1-2".into()]);
        assert!(rt.pending.lock().unwrap().is_empty(), "flushed");

        // Out-of-dialog request: never forwarded.
        let invite_new = "INVITE sip:b@x SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK2\r\nFrom: <sip:a@x>\r\nTo: <sip:b@y>\r\nCall-ID: brand-new-call\r\nCSeq: 1 INVITE\r\n\r\n";
        assert!(!rt.pump_inbound("10.0.0.9:5060".parse().unwrap(), invite_new.as_bytes()).await);

        // Response for a call the successor owns (not in manifest): local.
        let resp_new = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK3\r\nFrom: <sip:a@x>\r\nTo: <sip:b@y>;tag=t9\r\nCall-ID: myown-call-1\r\nCSeq: 1 INVITE\r\n\r\n";
        assert!(!rt.pump_inbound("10.0.0.9:5060".parse().unwrap(), resp_new.as_bytes()).await);

        // Response for a predecessor call: forwarded.
        let resp_old = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK4\r\nFrom: <sip:a@x>\r\nTo: <sip:b@y>;tag=t8\r\nCall-ID: oldcall-1-2\r\nCSeq: 9 OPTIONS\r\n\r\n";
        assert!(rt.pump_inbound("10.0.0.9:5060".parse().unwrap(), resp_old.as_bytes()).await);
    }
}
// ── tests ───────────────────────────────────────────────────────────────────
