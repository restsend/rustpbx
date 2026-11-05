//! Worker-side master link for `--master <sock>` mode.
//!
//! In worker mode the process binds no public ports. The master:
//! 1. Forwards UDP datagrams as `FrameKind::UdpUp` frames on the data channel
//! 2. Forwards TCP/HTTP connections as `TcpConn` + SCM_RIGHTS fd on the control channel
//!
//! The worker registers SIP transports dynamically via `add_connection`
//! (auto-starts serve loop + endpoint dispatch), same pattern as `ws.rs`.

use rustpbx_master_ipc::{
    ConnKind, ControlMessage, ControlSender, DataWriter, FrameKind, LinkHandle, worker_connect,
};
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::transport::Transport;
use rsipstack::transport::{
    channel::ChannelConnection,
    SipAddr, SipConnection, TransportEvent,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

// ── public types ──

pub trait DialogIdSource: Send + Sync + 'static {
    fn live_dialog_ids(&self) -> Vec<String>;
}

/// rsipstack Transport → the master-side ConnKind for routing tables.
pub fn conn_kind_of_transport(t: Transport) -> Option<ConnKind> {
    match t {
        Transport::Tcp => Some(ConnKind::SipTcp),
        Transport::Tls => Some(ConnKind::SipTls),
        Transport::Ws => Some(ConnKind::Ws),
        Transport::Wss => Some(ConnKind::Wss),
        _ => None,
    }
}

pub fn transport_of_conn_kind(kind: ConnKind) -> Transport {
    match kind {
        ConnKind::SipTcp => Transport::Tcp,
        ConnKind::SipTls => Transport::Tls,
        ConnKind::Ws => Transport::Ws,
        ConnKind::Wss => Transport::Wss,
    }
}

/// The wire Call-ID portion of a rsipstack DialogId string
/// (`{call_id}-{local_tag}-{remote_tag}`): everything before the first `-`.
/// Same rule as the master's `wire_call_id` — the master's DialogTable and
/// this mirror must derive IDENTICAL bare keys or relay decisions diverge.
fn wire_call_id_of(dialog_id: &str) -> Option<&str> {
    dialog_id.split_once('-').map(|(c, _)| c)
}

/// Injection path into a REAL connection this worker owns (conn-owner side
/// of the relay): a relayed message from another generation is delivered to
/// the client through the connection's own write path.
pub enum RealWrite {
    /// SIP over WebSocket — inject as a `TransportEvent::Incoming` on the
    /// conn's outbound channel; the ws write loop renders it as a text frame.
    WsChannel {
        to_ws: tokio::sync::mpsc::UnboundedSender<TransportEvent>,
        sip_connection: SipConnection,
        sip_addr: SipAddr,
    },
    /// SIP over a raw stream (TCP/TLS bridge) — framed bytes.
    Bytes(tokio::sync::mpsc::UnboundedSender<Vec<u8>>),
}

impl RealWrite {
    fn inject(&self, data: &[u8]) -> bool {
        match self {
            RealWrite::WsChannel { to_ws, sip_connection, sip_addr } => {
                match rsipstack::sip::SipMessage::try_from(data) {
                    Ok(msg) => to_ws
                        .send(TransportEvent::Incoming(
                            msg,
                            sip_connection.clone(),
                            sip_addr.clone(),
                        ))
                        .is_ok(),
                    Err(_) => false,
                }
            }
            RealWrite::Bytes(tx) => tx.send(data.to_vec()).is_ok(),
        }
    }
}

struct VirtualConn {
    token: CancellationToken,
    sip_connection: SipConnection,
    to_sip: tokio::sync::mpsc::UnboundedSender<TransportEvent>,
    sip_addr: SipAddr,
}

/// Per-generation state filled once `start_worker_mode` establishes the link
/// (and re-swapped transparently across reconnects through [`LinkWriters`]).
struct RelayLive {
    generation: u32,
    writers: Arc<LinkWriters>,
    /// Reach the transport layer THROUGH the endpoint ref — never hold a
    /// `TransportLayer` clone (its Drop cancels the shared transport token;
    /// see ws.rs WsChannelCleanupGuard for the full postmortem).
    endpoint: rsipstack::transaction::endpoint::EndpointInnerRef,
    /// Dialog-owner mirror: call_id → owning generation (OTHER gens only,
    /// maintained via master broadcasts). The relay-vs-local decision for
    /// messages read from REAL connections.
    mirror: dashmap::DashMap<String, u32>,
    /// Virtual relay connections WE hold (dialog-owner side): (kind, peer).
    virtual_conns: dashmap::DashMap<(u8, SocketAddr), Arc<VirtualConn>>,
    /// Real connection write paths WE own (conn-owner side): (kind, peer).
    real_writes: dashmap::DashMap<(u8, SocketAddr), RealWrite>,
    /// Close knobs for the real connections WE own (drain-time eviction).
    real_closes: dashmap::DashMap<(u8, SocketAddr), Arc<dyn Fn() + Send + Sync>>,
    conn_ids: AtomicU64,
    mirror_cap: usize,
    /// Pending RegistryLookup RPCs (nonce → reply channel).
    pending_lookups:
        dashmap::DashMap<u64, tokio::sync::oneshot::Sender<Option<(ConnKind, SocketAddr)>>>,
    lookup_nonce: AtomicU64,
}

/// Injected handle for worker-mode cross-generation SIP relay + connection
/// reporting. Created at builder time (before AppState exists), filled by
/// [`start_worker_mode`]; inert until then and in non-worker processes
/// (`live()` = None → every method no-ops).
#[derive(Clone, Default)]
pub struct WorkerRelay {
    live: Arc<std::sync::OnceLock<Arc<RelayLive>>>,
}

const MIRROR_CAP: usize = 50_000;

impl WorkerRelay {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { live: Arc::new(std::sync::OnceLock::new()) })
    }

    fn live(&self) -> Option<&Arc<RelayLive>> {
        self.live.get()
    }

    fn install(&self, live: Arc<RelayLive>) {
        let _ = self.live.set(live);
    }

    // ── connection reporting (ws.rs / stream bridges) ──

    /// Register a live SIP connection with the master's ConnTable. Returns
    /// the conn_id to hand to [`Self::report_conn_down`].
    pub fn report_conn_up(
        &self,
        kind: ConnKind,
        peer: SocketAddr,
        local: SocketAddr,
    ) -> Option<u64> {
        let live = self.live()?;
        let conn_id = live.conn_ids.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(control) = live.writers.control() {
            let _ = control.send(ControlMessage::ConnUp { conn_id, kind, peer, local });
        }
        Some(conn_id)
    }

    pub fn report_conn_down(&self, conn_id: u64) {
        let Some(live) = self.live() else { return };
        if let Some(control) = live.writers.control() {
            let _ = control.send(ControlMessage::ConnDown { conn_id });
        }
    }

    pub fn register_real_write(&self, kind: ConnKind, peer: SocketAddr, write: RealWrite) {
        let Some(live) = self.live() else { return };
        live.real_writes.insert((kind.as_u8(), peer), write);
    }

    pub fn unregister_real_write(&self, kind: ConnKind, peer: SocketAddr) {
        let Some(live) = self.live() else { return };
        live.real_writes.remove(&(kind.as_u8(), peer));
    }

    pub fn holds_real_conn(&self, kind: ConnKind, peer: SocketAddr) -> bool {
        let Some(live) = self.live() else { return false };
        live.real_writes.contains_key(&(kind.as_u8(), peer))
    }

    /// Register how to CLOSE a real connection this worker owns (drain-time
    /// idle eviction). `close` must fully tear the flow down — its own
    /// teardown path (guard / read-loop exit) performs the standard
    /// unregister + ConnDown sequence.
    pub fn register_real_close(&self, kind: ConnKind, peer: SocketAddr, close: Arc<dyn Fn() + Send + Sync>) {
        let Some(live) = self.live() else { return };
        live.real_closes.insert((kind.as_u8(), peer), close);
    }

    pub fn unregister_real_close(&self, kind: ConnKind, peer: SocketAddr) {
        let Some(live) = self.live() else { return };
        live.real_closes.remove(&(kind.as_u8(), peer));
    }

    /// Evict real connections this worker owns whose peer is NOT in `busy`
    /// (the set of live dialog remote targets). Draining generations call
    /// this periodically so registered-but-idle clients reconnect onto the
    /// current generation instead of pinning relay traffic on a draining
    /// process. Connections skipped from `busy` (live calls) keep flowing.
    pub fn evict_conns_except(&self, busy: &std::collections::HashSet<(String, u16)>) -> usize {
        let Some(live) = self.live() else { return 0 };
        let mut evicted = 0;
        let keys: Vec<(u8, SocketAddr)> = live
            .real_writes
            .iter()
            .map(|e| *e.key())
            .collect();
        for key in keys {
            let busy_hit = busy.contains(&(key.1.ip().to_string(), key.1.port()));
            if busy_hit {
                continue;
            }
            if let Some((_, close)) = live.real_closes.remove(&key) {
                info!(peer = %key.1, "drain: evicting idle real connection");
                close();
                evicted += 1;
            }
        }
        evicted
    }

    // ── inbound relay decision (bridge pumps scan REAL-conn messages) ──

    /// `Some(gen)` when the dialog belongs to ANOTHER generation → the
    /// message must be relayed, not processed locally. Mirror miss (or self)
    /// → `None` → local processing.
    pub fn foreign_dialog_owner(&self, call_id: &str) -> Option<u32> {
        let live = self.live()?;
        let owner = *live.mirror.get(call_id)?;
        (owner != live.generation).then_some(owner)
    }

    /// Relay a message read from a REAL connection to its dialog-owner
    /// generation (master routes by DialogTable / scanned Call-ID).
    pub fn relay_inbound(&self, kind: ConnKind, peer: SocketAddr, data: &[u8]) -> bool {
        let Some(live) = self.live() else { return false };
        let Some(data_writer) = live.writers.data() else { return false };
        let frame = rustpbx_master_ipc::frame::encode_datagram(
            FrameKind::RelayToDialog,
            kind.as_u8() as u16,
            &peer,
            data,
        );
        data_writer.send(frame)
    }

    /// Decision helper for bridge pumps: relay the message if its dialog is
    /// owned by another generation. Returns true when RELAYED (caller must
    /// skip local forwarding).
    pub fn maybe_relay_inbound(
        &self,
        msg: &rsipstack::sip::SipMessage,
        kind: ConnKind,
        peer: SocketAddr,
    ) -> bool {
        let Some(live) = self.live() else { return false };
        let Some(call_id) = msg.call_id_header().ok().map(|c| c.value().to_string()) else {
            debug!(%peer, ?kind, "relay scan: message without Call-ID — local");
            return false;
        };
        let owner = self.foreign_dialog_owner(&call_id);
        debug!(%call_id, %peer, ?kind, owner = ?owner, me = live.generation, mirror = live.mirror.len(), "relay scan decision");
        let Some(owner) = owner else {
            return false;
        };
        let raw = msg.to_string();
        if self.relay_inbound(kind, peer, raw.as_bytes()) {
            debug!(%call_id, %peer, ?kind, owner_gen = owner, "relayed in-dialog message to dialog-owner generation");
            true
        } else {
            warn!(%call_id, %peer, ?kind, "relay inbound: data writer unavailable");
            false
        }
    }

    // ── outbound interception (DialogTargetLocator / session dispatch) ──

    /// Make a resolved destination dialable across generations. For a
    /// stream-transport destination with no LOCAL real connection, create
    /// the virtual relay connection (idempotent; the real-conn case is
    /// detected inside and skipped). UDP and non-worker modes are untouched.
    ///
    /// Called from two seams: `DialogTargetLocator::locate` (locator-driven
    /// sends) and the session dispatch sites that pre-set
    /// `InviteOption.destination` (which SHORT-CIRCUITS `resolve_target_uri`
    /// and therefore never consults the locator — session.rs).
    pub async fn ensure_dialable(&self, dest: &SipAddr) {
        use rsipstack::sip::transport::Transport;
        let Some(transport) = dest.r#type else { return };
        let kind = match transport {
            Transport::Tcp => ConnKind::SipTcp,
            Transport::Tls => ConnKind::SipTls,
            Transport::Ws => ConnKind::Ws,
            Transport::Wss => ConnKind::Wss,
            _ => return,
        };
        // Registered destinations carry the client's ip:port; a domain host
        // is not a relayable flow (real remote server — rsipstack dials it).
        let peer = match &dest.addr.host {
            rsipstack::sip::Host::IpAddr(ip) => {
                SocketAddr::new(*ip, dest.addr.port.map(|p| p.0).unwrap_or(0))
            }
            _ => return,
        };
        self.ensure_virtual_conn(kind, peer).await;
    }

    /// Ensure a virtual relay connection to `(kind, peer)` exists in the
    /// local transport layer so rsipstack lookups find it. No-op when the
    /// real connection is local (nothing to relay) or already created.
    pub async fn ensure_virtual_conn(&self, kind: ConnKind, peer: SocketAddr) -> bool {
        let Some(live) = self.live() else { return false };
        let key = (kind.as_u8(), peer);
        if live.virtual_conns.contains_key(&key) {
            return true;
        }
        if live.real_writes.contains_key(&key) {
            return false; // the real conn is ours — rsipstack will find it
        }
        let sip_addr = SipAddr {
            r#type: Some(transport_of_conn_kind(kind)),
            addr: peer.into(),
        };
        let (to_sip_tx, to_sip_rx) = tokio::sync::mpsc::unbounded_channel();
        let (from_sip_tx, mut from_sip_rx) = tokio::sync::mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let Ok(conn) = ChannelConnection::create_connection(
            to_sip_rx,
            from_sip_tx,
            sip_addr.clone(),
            Some(token.clone()),
        )
        .await else {
            warn!(%peer, ?kind, "virtual relay connection creation failed");
            return false;
        };
        let sip_connection = SipConnection::Channel(conn);
        // Outbound pump: endpoint sends via the virtual conn → RelayToConn.
        let writers = live.writers.clone();
        tokio::spawn(async move {
            while let Some(event) = from_sip_rx.recv().await {
                if let TransportEvent::Incoming(ref msg, ..) = event {
                    let raw = msg.to_string();
                    if let Some(data_writer) = writers.data() {
                        let frame = rustpbx_master_ipc::frame::encode_datagram(
                            FrameKind::RelayToConn,
                            kind.as_u8() as u16,
                            &peer,
                            raw.as_bytes(),
                        );
                        let _ = data_writer.send(frame);
                    }
                }
            }
        });
        live.endpoint.transport_layer.add_connection(sip_connection.clone());
        live.virtual_conns.insert(
            key,
            Arc::new(VirtualConn {
                token: token.clone(),
                sip_connection: sip_connection.clone(),
                to_sip: to_sip_tx,
                sip_addr,
            }),
        );
        if let Some(control) = live.writers.control() {
            let _ = control.send(ControlMessage::RelayPairOpen { kind, peer });
        }
        info!(%peer, ?kind, "virtual relay connection registered");
        true
    }

    // ── Registry-in-Master (memory-locator deployments) ──

    /// Report an accepted registration to the master's node-level directory
    /// (`expires_secs == 0` removes the keys on deregistration).
    pub fn report_registry(
        &self,
        aors: &[String],
        kind: ConnKind,
        peer: Option<SocketAddr>,
        expires_secs: u64,
    ) {
        let Some(live) = self.live() else { return };
        if let Some(control) = live.writers.control() {
            let _ = control.send(ControlMessage::RegistryUp {
                aors: aors.to_vec(),
                kind,
                peer,
                expires_secs,
            });
        }
    }

    /// Dispatch-time directory query: ask the master which generation (and
    /// destination) holds `aor`. `None` on miss/timeout — the caller keeps
    /// its own fallback path.
    pub async fn registry_lookup(&self, aor: &str) -> Option<(ConnKind, SocketAddr)> {
        let Some(live) = self.live() else { return None };
        let Some(control) = live.writers.control() else { return None };
        let nonce = live.lookup_nonce.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = tokio::sync::oneshot::channel();
        live.pending_lookups.insert(nonce, tx);
        let sent = control.send(ControlMessage::RegistryLookup { nonce, aor: aor.to_string() });
        if sent.is_err() {
            live.pending_lookups.remove(&nonce);
            return None;
        }
        match tokio::time::timeout(Duration::from_millis(300), rx).await {
            Ok(Ok(reply)) => reply,
            _ => {
                live.pending_lookups.remove(&nonce);
                debug!(%aor, "registry lookup timed out");
                None
            }
        }
    }

    /// Reply from the master for a pending [`Self::registry_lookup`].
    fn resolve_lookup(&self, nonce: u64, dest: Option<(ConnKind, SocketAddr)>) {
        let Some(live) = self.live() else { return };
        if let Some((_, tx)) = live.pending_lookups.remove(&nonce) {
            let _ = tx.send(dest);
        }
    }

    // ── master → worker handlers (run_link pumps) ──

    /// Pre-announce a dialog being dispatched: report it to the master's
    /// DialogTable BEFORE the INVITE hits the wire. The reporter loop polls
    /// `dialog_layer` at 1s cadence — an echo-fast callee answers within
    /// milliseconds, and its response arrives at the conn-owner generation
    /// before the announce does; the conn-owner scan would then miss the
    /// mirror and drop the response locally. Pre-announcing closes that
    /// window at the dispatch site (the dialog id is authoritative — if the
    /// leg dies pre-answer, DialogDown cleans it up).
    pub fn preannounce_dialog(&self, call_id: &str) {
        let Some(live) = self.live() else { return };
        if let Some(control) = live.writers.control() {
            let _ = control.send(ControlMessage::DialogUp { call_id: call_id.to_string() });
        }
    }

    fn apply_mirror_announce(&self, call_id: String, generation: u32) {
        let Some(live) = self.live() else { return };
        if generation == live.generation {
            live.mirror.remove(&call_id);
            if let Some(bare) = wire_call_id_of(&call_id) {
                live.mirror.remove(bare);
            }
            return;
        }
        if live.mirror.len() >= live.mirror_cap {
            warn!("dialog-owner mirror hit its cap — clearing (MirrorSnapshot repopulates)");
            live.mirror.clear();
        }
        // Dual-key, mirroring the master's DialogTable: announcements carry
        // the full DialogId (`callid-local-remote`) but the bridge scan looks
        // up the BARE wire Call-ID — both must resolve to the owner gen.
        live.mirror.insert(call_id.clone(), generation);
        if let Some(bare) = wire_call_id_of(&call_id) {
            live.mirror.insert(bare.to_string(), generation);
        }
        debug!(%call_id, %generation, size = live.mirror.len(), me = live.generation, "mirror announce applied");
    }

    fn apply_mirror_gone(&self, call_id: &str) {
        let Some(live) = self.live() else { return };
        live.mirror.remove(call_id);
        if let Some(bare) = wire_call_id_of(call_id) {
            live.mirror.remove(bare);
        }
    }

    fn apply_mirror_snapshot(&self, entries: Vec<(String, u32)>) {
        let Some(live) = self.live() else { return };
        live.mirror.clear();
        for (call_id, generation) in entries {
            if generation != live.generation {
                live.mirror.insert(call_id, generation);
            }
        }
        debug!(size = live.mirror.len(), "dialog-owner mirror rebuilt");
    }

    /// A relayed SIP message arrived for local injection: into the REAL
    /// connection we own (conn-owner), or as inbound on the virtual relay
    /// connection (dialog-owner — created lazily so rsipstack affinity
    /// fallback and responses find it).
    pub async fn handle_relay_deliver(&self, kind: ConnKind, peer: SocketAddr, data: &[u8]) {
        let Some(live) = self.live() else { return };
        let key = (kind.as_u8(), peer);
        if let Some(rw) = live.real_writes.get(&key) {
            let injected = rw.inject(data);
            debug!(%peer, ?kind, injected, bytes = data.len(), "relay deliver: real-conn injection");
            if injected {
                return;
            }
            // injection failed (conn is dying) — fall through; the virtual
            // path is pointless too, drop.
            return;
        }
        let vc = match live.virtual_conns.get(&key) {
            Some(v) => v.clone(),
            None => {
                if !self.ensure_virtual_conn(kind, peer).await {
                    return;
                }
                match live.virtual_conns.get(&key) {
                    Some(v) => v.clone(),
                    None => return,
                }
            }
        };
        match rsipstack::sip::SipMessage::try_from(data) {
            // Do NOT re-stamp received/rport: the message was already stamped
            // at the conn-owner gen; re-injection rides the virtual conn and
            // responses return through it (server transaction holds it).
            Ok(msg) => {
                let _ = vc.to_sip.send(TransportEvent::Incoming(
                    msg,
                    vc.sip_connection.clone(),
                    vc.sip_addr.clone(),
                ));
            }
            Err(e) => warn!(error = %e, "relay deliver: unparsable SIP payload"),
        }
    }

    /// The REAL connection `(kind, peer)` died in its owning generation:
    /// tear down our phantom virtual flow — token cancel + serve-loop exit
    /// emits `TransportEvent::Closed`, which drives the existing locator
    /// unbind and dialog failure paths. Nothing new is invented here.
    pub fn handle_conn_closed(&self, kind: ConnKind, peer: SocketAddr) {
        let Some(live) = self.live() else { return };
        let key = (kind.as_u8(), peer);
        if let Some((_, vc)) = live.virtual_conns.remove(&key) {
            vc.token.cancel();
            live.endpoint.transport_layer.remove_connection(&vc.sip_connection);
            if let Some(control) = live.writers.control() {
                let _ = control.send(ControlMessage::RelayPairClose { kind, peer });
            }
            info!(%peer, ?kind, "virtual relay connection closed (real conn died)");
        }
    }
}

// ── helpers ──

struct SipServerDialogs {
    inner: crate::proxy::server::SipServerRef,
}

impl DialogIdSource for SipServerDialogs {
    fn live_dialog_ids(&self) -> Vec<String> {
        self.inner
            .dialog_layer
            .all_dialog_ids()
            .into_iter()
            .filter(|id| {
                matches!(
                    self.inner.dialog_layer.get_dialog_with(id),
                    Some(rsipstack::dialog::dialog::Dialog::Invite(_))
                )
            })
            .collect()
    }
}

fn build_tls_acceptor(state: &crate::app::AppState) -> Option<tokio_rustls::TlsAcceptor> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let config = state.config();
    let (cert, key) = (&config.ssl_certificate, &config.ssl_private_key);
    let (cert, key) = (cert.as_deref()?, key.as_deref()?);
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
    Some(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

/// Response destination per RFC 3581: the request's Via was stamped with
/// `received=<real ip>;rport=<real port>` by the bridge (update_msg_received
/// saw the REAL source through the master relay). Prefer received+rport —
/// the sent-by address is what the UA *claims* and is wrong behind NAT —
/// falling back to sent-by for non-NAT/no-rport clients.
fn response_dest_from_via(raw: &str) -> Option<SocketAddr> {
    let via = raw.lines().find(|l| l.to_ascii_lowercase().starts_with("via:"))?;
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

fn futures_block<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

// ── UDP relay via ChannelConnection ──

/// Create a ChannelConnection for a relay UDP listener and register it.
fn setup_udp_relay(
    tl: &rsipstack::transport::TransportLayer,
    listener_id: u16,
    local: SocketAddr,
    writers: Arc<LinkWriters>,
) -> Option<tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>> {
    let sip_addr = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Udp),
        addr: local.into(),
    };
    let (to_sip_tx, to_sip_rx) = tokio::sync::mpsc::unbounded_channel();
    let (from_sip_tx, mut from_sip_rx) = tokio::sync::mpsc::unbounded_channel();

    let conn = futures_block(async {
        ChannelConnection::create_connection(to_sip_rx, from_sip_tx, sip_addr.clone(), None).await
    }).ok()?;

    // Outgoing pump: SIP messages → master → public socket.
    // Destination rule: RESPONSES follow the Via sent-by (back along the
    // path they came); REQUESTS go to the R-URI host:port — using Via for
    // requests would target the master's own public address (the advertised
    // sent-by) and loop the packet back into the worker.
    let lid = listener_id;
    tokio::spawn(async move {
        while let Some(event) = from_sip_rx.recv().await {
            let Some(data) = writers.data() else { continue };
            if let TransportEvent::Incoming(ref msg, _, _) = event {
                let raw = msg.to_string();
                let dst = match msg {
                    rsipstack::sip::SipMessage::Request(req) => {
                        let hwp = &req.uri.host_with_port;
                        format!(
                            "{}:{}",
                            hwp.host,
                            hwp.port.map(|p| p.to_string()).unwrap_or_else(|| "5060".into())
                        )
                        .parse()
                        .ok()
                    }
                    _ => response_dest_from_via(&raw),
                };
                if let Some(dst) = dst {
                    let frame = rustpbx_master_ipc::frame::encode_datagram(
                        FrameKind::UdpDown, lid, &dst, raw.as_bytes(),
                    );
                    let _ = data.send(frame);
                }
            }
        }
    });

    // Bridge: raw datagrams from data pump → TransportEvent → to_sip_tx
    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<(Vec<u8>, SocketAddr)>();
    let to_sip = to_sip_tx.clone();
    let conn_for_bridge = conn.clone();
    tokio::spawn(async move {
        while let Some((datagram, src)) = raw_rx.recv().await {
            match rsipstack::sip::SipMessage::try_from(datagram.as_slice()) {
                Ok(msg) => {
                    let msg = match rsipstack::transport::SipConnection::update_msg_received(
                        msg, src, rsipstack::sip::transport::Transport::Udp,
                    ) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let event = TransportEvent::Incoming(
                        msg, SipConnection::Channel(conn_for_bridge.clone()), sip_addr.clone(),
                    );
                    if to_sip.send(event).is_err() { break; }
                }
                Err(_) => {}
            }
        }
    });

    // Register (auto-starts serve loop + endpoint dispatch)
    tl.add_connection(SipConnection::Channel(conn));
    tracing::info!(listener_id, %local, "relay UDP registered");
    Some(raw_tx)
}

// ── HTTP fd serving ──

struct InjectPeer {
    inner: axum::Router,
    peer: SocketAddr,
}

impl Clone for InjectPeer {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), peer: self.peer }
    }
}

impl tower::Service<axum::http::Request<hyper::body::Incoming>> for InjectPeer {
    type Response = <axum::Router as tower::Service<axum::http::Request<hyper::body::Incoming>>>::Response;
    type Error = <axum::Router as tower::Service<axum::http::Request<hyper::body::Incoming>>>::Error;
    type Future = <axum::Router as tower::Service<axum::http::Request<hyper::body::Incoming>>>::Future;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        tower::Service::<axum::http::Request<hyper::body::Incoming>>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, mut req: axum::http::Request<hyper::body::Incoming>) -> Self::Future {
        req.extensions_mut().insert(axum::extract::ConnectInfo(self.peer));
        tower::Service::<axum::http::Request<hyper::body::Incoming>>::call(&mut self.inner, req)
    }
}

fn serve_http(fd: std::os::fd::OwnedFd, peer: SocketAddr, router: axum::Router) {
    let stream = std::net::TcpStream::from(fd);
    let _ = stream.set_nonblocking(true);
    let Ok(stream) = tokio::net::TcpStream::from_std(stream) else { return };
    serve_http_stream(stream, peer, router);
}

fn serve_http_stream(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    peer: SocketAddr,
    router: axum::Router,
) {
    tokio::spawn(async move {
        use hyper_util::rt::TokioIo;
        let svc = hyper_util::service::TowerToHyperService::new(InjectPeer { inner: router, peer });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), svc)
            .with_upgrades()
            .await;
    });
}

// ── start_worker_mode (reconnect-capable) ──

/// Writers shared across reconnects: the dialog reporter and the UDP relay
/// pumps keep running while the underlying link is swapped underneath them.
#[derive(Default)]
struct LinkWriters {
    control: std::sync::RwLock<Option<ControlSender>>,
    data: std::sync::RwLock<Option<DataWriter>>,
    version: std::sync::atomic::AtomicU64,
}

impl LinkWriters {
    fn set(&self, control: &ControlSender, data: &DataWriter) {
        *self.control.write().unwrap() = Some(control.clone());
        *self.data.write().unwrap() = Some(data.clone());
        self.version.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    fn control(&self) -> Option<ControlSender> {
        self.control.read().unwrap().clone()
    }
    fn data(&self) -> Option<DataWriter> {
        self.data.read().unwrap().clone()
    }
    fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Start worker mode: install transports, report dialogs, handle control —
/// and RECONNECT to the master if the link ever dies (master restart/crash),
/// re-announcing the SAME generation until adopted or stopped.
pub fn start_worker_mode(
    link: LinkHandle,
    generation: u32,
    master_sock: PathBuf,
    state: crate::app::AppState,
    router: axum::Router,
    relay: Arc<WorkerRelay>,
) {
    let dialogs: Arc<dyn DialogIdSource> = Arc::new(SipServerDialogs {
        inner: state.sip_server().inner.clone(),
    });
    let tls_acceptor = build_tls_acceptor(&state);
    let relays: Arc<dashmap::DashMap<u16, tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>>> =
        Arc::new(dashmap::DashMap::new());
    let writers = Arc::new(LinkWriters::default());
    relay.install(Arc::new(RelayLive {
        generation,
        writers: writers.clone(),
        endpoint: state.sip_server().inner.endpoint.inner.clone(),
        mirror: dashmap::DashMap::new(),
        virtual_conns: dashmap::DashMap::new(),
        real_writes: dashmap::DashMap::new(),
        conn_ids: AtomicU64::new(0),
        mirror_cap: MIRROR_CAP,
        pending_lookups: dashmap::DashMap::new(),
        lookup_nonce: AtomicU64::new(0),
        real_closes: dashmap::DashMap::new(),
    }));

    tokio::spawn(async move {
        // ── dialog diff reporter: ONE instance across reconnects ──
        {
            let writers = writers.clone();
            let dialogs = dialogs.clone();
            tokio::spawn(async move {
                let mut last: HashSet<String> = Default::default();
                let mut seen_version = 0u64;
                let mut tick: u32 = 0;
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let Some(reporter) = writers.control() else { continue };
                    // Fresh link (or fresh master): resend the full live set —
                    // the far side may have lost its routing table.
                    let version = writers.version();
                    let full_resend = version != seen_version;
                    seen_version = version;
                    let live = dialogs.live_dialog_ids();
                    let live_set: HashSet<String> = live.iter().cloned().collect();
                    for id in &live {
                        if full_resend || !last.contains(id) {
                            let _ = reporter.send(ControlMessage::DialogUp { call_id: id.clone() });
                        }
                    }
                    if !full_resend {
                        for id in &last {
                            if !live_set.contains(id) {
                                let _ = reporter.send(ControlMessage::DialogDown { call_id: id.clone() });
                            }
                        }
                    }
                    tick += 1;
                    if tick % 5 == 0 {
                        let _ = reporter.send(ControlMessage::Stats { active_dialogs: live.len() as u64 });
                    }
                    if tick % 30 == 0 || full_resend {
                        let _ = reporter.send(ControlMessage::Snapshot { call_ids: live.clone() });
                    }
                    last = live_set;
                }
            });
        }

        // ── link supervision loop ──
        let mut current = Some(link);
        loop {
            {
                let link = current.take().expect("link present");
                writers.set(&link.control_tx, &link.data);
                run_link(
                    link,
                    generation,
                    writers.clone(),
                    relays.clone(),
                    state.clone(),
                    router.clone(),
                    tls_acceptor.clone(),
                    relay.clone(),
                )
                .await;
            }
            if state.token().is_cancelled() {
                tracing::info!("link lost after Stop — not reconnecting");
                break;
            }
            tracing::warn!("master link lost — reconnecting");
            let mut delay = Duration::from_millis(500);
            let mut reconnected = false;
            for attempt in 0..12u32 {
                tokio::time::sleep(delay).await;
                match worker_connect(&master_sock).await {
                    Ok(l) => {
                        current = Some(l);
                        reconnected = true;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(attempt, error = %e, "master reconnect failed");
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                }
            }
            if !reconnected {
                tracing::error!("master unreachable after 12 attempts — stopping worker");
                crate::shutdown::initiate();
                state.token().cancel();
                break;
            }
            tracing::info!("master link re-established — re-announcing generation {generation}");
        }
    });
}

/// One link's lifetime: Hello/Ready, control pump, UDP data pump. Returns
/// when the link dies or the master sends Stop (which cancels the token).
#[allow(clippy::too_many_arguments)]
async fn run_link(
    link: LinkHandle,
    generation: u32,
    writers: Arc<LinkWriters>,
    relays: Arc<dashmap::DashMap<u16, tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>>>,
    state: crate::app::AppState,
    router: axum::Router,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    relay: Arc<WorkerRelay>,
) {
    let state_ctrl = state.clone();
    let router_ctrl = router.clone();
    let mut control_rx = link.control_rx;

    // Identify ourselves FIRST: the master binds this link to the generation
    // it spawned (or adopts a survivor); without Hello it ignores Ready.
    // Hello+Ready sent BEFORE the reader pump starts — nothing else sends yet.
    let _ = writers.control().map(|c| {
        c.send(ControlMessage::Hello {
            generation,
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
        })
    });
    let _ = writers.control().map(|c| c.send(ControlMessage::Ready));

    // ── control pump ──
    let relays_ctrl = relays.clone();
    let relay_ctrl = relay.clone();
    let control_task = tokio::spawn(async move {
        let relay = relay_ctrl;
        while let Some(frame) = control_rx.recv().await {
            match frame.msg {
                ControlMessage::UdpBind { listener_id, local, .. } => {
                    // Relays persist across reconnects — register once.
                    if relays_ctrl.contains_key(&listener_id) {
                        continue;
                    }
                    if let Some(tx) = setup_udp_relay(&state_ctrl.sip_server().inner.endpoint.inner.transport_layer, listener_id, local, writers.clone()) {
                        relays_ctrl.insert(listener_id, tx);
                    }
                }
                ControlMessage::TcpConn { kind, local, peer, .. } => {
                    let mut fds = frame.fds;
                    let Some(fd) = fds.pop() else { continue };
                    match kind {
                        rustpbx_master_ipc::StreamKind::SipTcp => {
                            serve_sip_stream_fd(fd, Transport::Tcp, peer, local, relay.clone());
                        }
                        rustpbx_master_ipc::StreamKind::Http => {
                            serve_http(fd, peer, router_ctrl.clone());
                        }
                        rustpbx_master_ipc::StreamKind::SipTls => {
                            if let Some(acceptor) = &tls_acceptor {
                                let stream = std::net::TcpStream::from(fd);
                                let _ = stream.set_nonblocking(true);
                                let acceptor = acceptor.clone();
                                let relay_tls = relay.clone();
                                tokio::spawn(async move {
                                    let Ok(stream) = tokio::net::TcpStream::from_std(stream) else { return };
                                    match acceptor.accept(stream).await {
                                        Ok(tls) => {
                                            serve_sip_stream_tls(tls, Transport::Tls, peer, local, relay_tls).await;
                                        }
                                        Err(e) => tracing::warn!("sip tls accept: {e}"),
                                    }
                                });
                            }
                        }
                        rustpbx_master_ipc::StreamKind::HttpTls => {
                            if let Some(acceptor) = &tls_acceptor {
                                let stream = std::net::TcpStream::from(fd);
                                let _ = stream.set_nonblocking(true);
                                let acceptor = acceptor.clone();
                                let router = router_ctrl.clone();
                                tokio::spawn(async move {
                                    let Ok(stream) = tokio::net::TcpStream::from_std(stream) else { return };
                                    match acceptor.accept(stream).await {
                                        Ok(tls) => serve_http_stream(tls, peer, router),
                                        Err(e) => tracing::warn!("tls accept: {e}"),
                                    }
                                });
                            }
                        }
                    }
                }
                ControlMessage::DialogOwnerAnnounce { call_id, generation } => {
                    relay.apply_mirror_announce(call_id, generation);
                }
                ControlMessage::DialogOwnerGone { call_id } => {
                    relay.apply_mirror_gone(&call_id);
                }
                ControlMessage::MirrorSnapshot { entries } => {
                    relay.apply_mirror_snapshot(entries);
                }
                ControlMessage::ConnClosed { kind, peer } => {
                    relay.handle_conn_closed(kind, peer);
                }
                ControlMessage::RegistryLookupReply { nonce, dest } => {
                    relay.resolve_lookup(nonce, dest);
                }
                ControlMessage::Drain { .. } => {
                    tracing::info!("master requested drain");
                    // Same semantics as SIGTERM drain: reject new calls, wait
                    // for active dialogs to finish — but do NOT cancel the
                    // runtime token; the master sends Stop when it has seen
                    // Drained.
                    //
                    // The wait runs in a SEPARATE task: this pump must keep
                    // consuming control messages during the drain — the
                    // cross-gen relay depends on it (DialogOwnerAnnounce /
                    // ConnClosed for live dialogs still flowing through a
                    // draining generation). The old inline wait starved the
                    // pump for the whole drain window and with it every
                    // relayed response (observed as caller 408 in the e2e).
                    let first = crate::shutdown::initiate();
                    let timeout = state_ctrl.config().graceful_shutdown_timeout();
                    let state_drain = state_ctrl.clone();
                    let writers_drain = writers.clone();
                    let relay_drain = relay.clone();
                    if first {
                        let active_calls = state_drain.sip_server().inner.active_call_registry.count();
                        tracing::info!(
                            ?timeout,
                            active_calls,
                            "draining: rejecting new calls, waiting for dialogs to end"
                        );
                    }
                    tokio::spawn(async move {
                        let sip = state_drain.sip_server().clone();
                        // Progressive idle-conn eviction while draining: registered
                        // but call-less clients are disconnected so they reconnect
                        // onto the current generation (zero relay for them). Conns
                        // carrying live dialogs are skipped (remote-contact match).
                        let evict_relay = relay_drain.clone();
                        let sip_evict = sip.clone();
                        let evict_task = tokio::spawn(async move {
                            let mut ticker = tokio::time::interval(Duration::from_secs(2));
                            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                            loop {
                                ticker.tick().await;
                                let busy: std::collections::HashSet<(String, u16)> =
                                    sip_evict.inner.dialog_layer.all_dialog_ids().iter().filter_map(|id| {
                                        let dialog = sip_evict.inner.dialog_layer.get_dialog_with(id)?;
                                        let contact = dialog.remote_contact()?;
                                        Some((
                                            contact.host().to_string(),
                                            contact.host_with_port.port.map(|p| p.0).unwrap_or(0),
                                        ))
                                    })
                                    .collect();
                                evict_relay.evict_conns_except(&busy);
                            }
                        });
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
                        evict_task.abort();
                        crate::shutdown::log_drain_outcome(outcome);
                        let _ = writers_drain.control().map(|c| c.send(ControlMessage::Drained));
                    });
                }
                ControlMessage::Stop { .. } => {
                    tracing::info!("master requested stop");
                    crate::shutdown::initiate();
                    state_ctrl.token().cancel();
                    return;
                }
                _ => {}
            }
        }
        tracing::warn!("control channel closed — link dead");
    });

    // ── UDP data pump + relay deliver (per link) ──
    let relays_dp = relays.clone();
    let mut data_rx = link.data_rx;
    let data_task = tokio::spawn(async move {
        while let Some(df) = data_rx.recv().await {
            match df.kind {
                FrameKind::UdpUp => {
                    if let Some(tx) = relays_dp.get(&df.listener_id) {
                        let _ = tx.send((df.datagram, df.addr));
                    }
                }
                FrameKind::RelayDeliver => {
                    if let Some(kind) = rustpbx_master_ipc::frame::unpack_conn_kind(df.listener_id) {
                        relay.handle_relay_deliver(kind, df.addr, &df.datagram).await;
                    }
                }
                _ => {}
            }
        }
    });

    // The link's lifetime = the control channel's lifetime. run_link MUST
    // await it — returning early would drop the LinkHandle (killing the
    // connection) and trigger a reconnect storm.
    let _ = control_task.await;
    data_task.abort();
    tracing::info!(generation, "worker link dead");
}

/// Serve a master-forwarded TCP/TLS connection as a BRIDGED SIP stream
/// (rsipstack ChannelConnection + rustpbx-side pumps). Bridging (instead of
/// `TcpConnection::from_stream`) is what gives the worker-mode stream the
/// three seams the cross-gen relay needs:
/// * inbound scan → relay-to-dialog-owner decision per message,
/// * a `RealWrite` injection path (RelayDeliver from the conn-owner side),
/// * ConnUp/ConnDown reporting for the master's ConnTable.
async fn bridge_sip_stream<S>(
    stream: S,
    transport: Transport,
    peer: SocketAddr,
    local: SocketAddr,
    relay: Option<Arc<WorkerRelay>>,
    endpoint: rsipstack::transaction::endpoint::EndpointInnerRef,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let Some(kind) = conn_kind_of_transport(transport) else { return };
    let sip_addr = SipAddr {
        r#type: Some(transport),
        addr: peer.into(),
    };
    let (to_sip_tx, to_sip_rx) = tokio::sync::mpsc::unbounded_channel();
    let (from_sip_tx, mut from_sip_rx) = tokio::sync::mpsc::unbounded_channel();
    // Per-flow cancel token — REQUIRED, not optional: rsipstack's dialog
    // affinity (`resolve_affinity_connection`) treats a connection whose
    // token is cancelled as dead and falls back to a fresh transport-layer
    // lookup (where the cross-gen virtual relay connection lives). Without
    // a token, a dead bridge flow keeps winning affinity and in-dialog
    // sends die with 503 instead of riding the relay.
    let bridge_token = CancellationToken::new();
    let conn = match ChannelConnection::create_connection(
        to_sip_rx,
        from_sip_tx,
        sip_addr.clone(),
        Some(bridge_token.clone()),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(%peer, error = %e, "sip stream bridge: channel connection failed");
            return;
        }
    };
    let sip_connection = SipConnection::Channel(conn);
    endpoint.transport_layer.add_connection(sip_connection.clone());

    let conn_id = relay.as_ref().and_then(|r| r.report_conn_up(kind, peer, local));

    // ── writer task: owns the write half; single sink for endpoint-outbound
    // messages AND relay-injected bytes (ordered, no interleaving races).
    let (inject_tx, mut inject_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    if let Some(r) = relay.as_ref() {
        r.register_real_write(kind, peer, RealWrite::Bytes(inject_tx.clone()));
        let token_for_close = bridge_token.clone();
        r.register_real_close(
            kind,
            peer,
            Arc::new(move || token_for_close.cancel()),
        );
    }
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = inject_rx.recv().await {
            if write_half.write_all(&bytes).await.is_err() {
                break;
            }
            let _ = write_half.flush().await;
        }
    });
    // endpoint-outbound pump → writer
    let out_pump = tokio::spawn(async move {
        while let Some(event) = from_sip_rx.recv().await {
            if let TransportEvent::Incoming(ref msg, ..) = event {
                if inject_tx.send(msg.to_string().into_bytes()).is_err() {
                    break;
                }
            }
        }
    });

    // ── read loop: framed SIP, relay-vs-local decision per message ──
    // The flow token doubles as the drain-time eviction close knob.
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        let frame = tokio::select! {
            biased;
            _ = bridge_token.cancelled() => {
                debug!(%peer, ?kind, "sip stream bridge: flow token cancelled (evicted)");
                None
            }
            r = read_sip_frame(&mut read_half, &mut buf) => match r {
                Ok(Some(frame)) => Some(frame),
                Ok(None) => None,
                Err(e) => {
                    debug!(%peer, error = %e, "sip stream bridge: read error");
                    None
                }
            },
        };
        match frame {
            Some(frame) => {
                let msg = match rsipstack::sip::SipMessage::try_from(frame.as_slice()) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!(%peer, error = %e, "sip stream bridge: unparsable frame");
                        continue;
                    }
                };
                let msg = match SipConnection::update_msg_received(msg, peer, transport) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if let Some(r) = relay.as_ref() {
                    if r.maybe_relay_inbound(&msg, kind, peer) {
                        continue; // relayed to the dialog-owner generation
                    }
                }
                if to_sip_tx
                    .send(TransportEvent::Incoming(msg, sip_connection.clone(), sip_addr.clone()))
                    .is_err()
                {
                    break;
                }
            }
            None => break, // EOF or evicted
        }
    }

    // ── teardown: registration first, then kill the flow token (affinity
    // dead-marking + serve-loop exit) and drop the channel senders so the
    // transport layer emits Closed (locator unbind).
    if let Some(r) = relay.as_ref() {
        r.unregister_real_close(kind, peer);
        r.unregister_real_write(kind, peer);
        if let Some(id) = conn_id {
            r.report_conn_down(id);
        }
    }
    endpoint.transport_layer.remove_connection(&sip_connection);
    bridge_token.cancel();
    drop(to_sip_tx);
    out_pump.abort();
    writer_task.abort();
    debug!(%peer, ?kind, "sip stream bridge ended");
}

fn serve_sip_stream_fd(
    fd: std::os::fd::OwnedFd,
    transport: Transport,
    peer: SocketAddr,
    local: SocketAddr,
    relay: Arc<WorkerRelay>,
) {
    let stream = std::net::TcpStream::from(fd);
    let _ = stream.set_nonblocking(true);
    let Ok(stream) = tokio::net::TcpStream::from_std(stream) else { return };
    let Some(endpoint) = relay.live().map(|l| l.endpoint.clone()) else {
        return;
    };
    tokio::spawn(bridge_sip_stream(
        stream,
        transport,
        peer,
        local,
        Some(relay),
        endpoint,
    ));
}

/// TLS variant: terminate at the worker, then run the same bridge over the
/// cleartext stream (SIP bytes are what the relay needs to scan).
async fn serve_sip_stream_tls<S>(
    tls: S,
    transport: Transport,
    peer: SocketAddr,
    local: SocketAddr,
    relay: Arc<WorkerRelay>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let Some(endpoint) = relay.live().map(|l| l.endpoint.clone()) else { return };
    bridge_sip_stream(tls, transport, peer, local, Some(relay), endpoint).await;
}

/// SIP-over-TCP framing: headers end at `\r\n\r\n` (tolerating bare `\n`),
/// body length from `Content-Length` (0 when absent). Returns one complete
/// message; `Ok(None)` on EOF.
async fn read_sip_frame<S>(
    stream: &mut S,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<Vec<u8>>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    loop {
        if let Some(total) = sip_frame_len(buf) {
            // Headers complete but the body may still be short — only emit
            // once the FULL message (headers + Content-Length body) arrived.
            if buf.len() >= total {
                return Ok(Some(buf.drain(..total).collect()));
            }
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 1 << 20 {
            // 1 MB without a complete header block — not SIP.
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
        }
    }
}

/// Total message length (headers + body) when the buffered bytes contain a
/// complete header block, else `None`.
fn sip_frame_len(buf: &[u8]) -> Option<usize> {
    let header_end = find_header_end(buf)?;
    let headers = &buf[..header_end];
    let mut content_length: usize = 0;
    let mut pos = 0;
    while pos < headers.len() {
        let eol = headers[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| p + pos)
            .unwrap_or(headers.len());
        let line = &headers[pos..eol];
        let trimmed = if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { line };
        if let Some(v) = parse_content_length(trimmed) {
            content_length = v;
        }
        pos = eol + 1;
    }
    Some(header_end + content_length)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' {
            if buf[i + 1] == b'\n' {
                return Some(i + 2);
            }
            if i + 2 < buf.len() && buf[i + 1] == b'\r' && buf[i + 2] == b'\n' {
                return Some(i + 3);
            }
        }
    }
    None
}

fn parse_content_length(line: &[u8]) -> Option<usize> {
    let lower: Vec<u8> = line.iter().map(|b| b.to_ascii_lowercase()).collect();
    let rest = if lower.starts_with(b"content-length:") {
        &line[b"content-length:".len()..]
    } else if lower.starts_with(b"l:") {
        &line[2..]
    } else {
        return None;
    };
    let text = std::str::from_utf8(rest).ok()?;
    text.trim().parse().ok()
}

/// Direct rsipstack-pumped serve (non-worker fallback — no relay seams).
#[allow(dead_code)]
fn serve_sip_tcp_fd_direct(
    tl: &rsipstack::transport::TransportLayer,
    fd: std::os::fd::OwnedFd,
    local: SocketAddr,
) {
    let stream = std::net::TcpStream::from(fd);
    let _ = stream.set_nonblocking(true);
    let Ok(stream) = tokio::net::TcpStream::from_std(stream) else { return };
    let addr = SipAddr {
        r#type: Some(Transport::Tcp),
        addr: local.into(),
    };
    match rsipstack::transport::tcp::TcpConnection::from_stream(stream, addr, None) {
        Ok(conn) => { tl.add_connection(SipConnection::Tcp(conn)); }
        Err(e) => tracing::warn!("TcpConnection::from_stream: {e}"),
    }
}

#[cfg(test)]
mod via_routing_tests {
    use super::*;

    /// NAT 场景：UA 自报私网地址，桥接盖了 received/rport —— 响应必须回
    /// 真实源地址（ACL/远端 IP 同源机制），绝不能发给 UA 自报的私网地址。
    #[test]
    fn response_goes_to_received_rport_behind_nat() {
        let raw = "SIP/2.0 200 OK\r\n\
                   Via: SIP/2.0/UDP 10.0.0.8:5060;received=203.0.113.10;rport=51442;branch=z9hG4bK nat\r\n\
                   From: <sip:u@x>;tag=1\r\nTo: <sip:e@x>;tag=2\r\n";
        let dst = response_dest_from_via(raw).expect("dest");
        assert_eq!(dst.ip().to_string(), "203.0.113.10");
        assert_eq!(dst.port(), 51442);
    }

    /// 非 NAT：无 received，sent-by 即真实地址。
    #[test]
    fn response_falls_back_to_sent_by() {
        let raw = "SIP/2.0 200 OK\r\n\
                   Via: SIP/2.0/UDP 192.168.3.9:50952;branch=z9hG4bK x\r\n";
        let dst = response_dest_from_via(raw).expect("dest");
        assert_eq!(dst.to_string(), "192.168.3.9:50952");
    }

    /// rport 有回显但无 received（同址不同端口）：host 用 sent-by，port 用 rport。
    #[test]
    fn rport_only_overrides_port() {
        let raw = "SIP/2.0 200 OK\r\n\
                   Via: SIP/2.0/UDP 192.168.3.9:5060;rport=51442;branch=z9 x\r\n";
        let dst = response_dest_from_via(raw).expect("dest");
        assert_eq!(dst.to_string(), "192.168.3.9:51442");
    }

    /// 请求路径：按 R-URI 路由（in-dialog 请求指向对端 contact）。
    #[test]
    fn request_routes_by_ruri() {
        let raw = "BYE sip:echo1@127.0.0.1:36463 SIP/2.0\r\n\
                   Via: SIP/2.0/UDP 127.0.0.1:5010;branch=z9 x\r\n\
                   Max-Forwards: 70\r\n\r\n";
        // 与出向 pump 相同的解析路径：datagram → SipMessage → R-URI host:port
        let dst = match rsipstack::sip::SipMessage::try_from(raw.as_bytes())
            .expect("parse BYE")
        {
            rsipstack::sip::SipMessage::Request(req) => {
                let hwp = &req.uri.host_with_port;
                format!(
                    "{}:{}",
                    hwp.host,
                    hwp.port.map(|p| p.to_string()).unwrap_or_else(|| "5060".into())
                )
                .parse::<SocketAddr>()
                .expect("R-URI dest")
            }
            _ => panic!("not a request"),
        };
        assert_eq!(dst.to_string(), "127.0.0.1:36463");
    }
}

#[cfg(test)]
mod relay_tests {
    use super::*;

    // ── SIP-over-TCP framing ──

    fn frame_len_of(buf: &[u8]) -> Option<usize> {
        sip_frame_len(buf)
    }

    #[test]
    fn complete_frame_with_body() {
        let msg = b"INVITE sip:x SIP/2.0\r\nContent-Length: 4\r\n\r\nBODY";
        assert_eq!(frame_len_of(msg), Some(msg.len()));
    }

    #[test]
    fn partial_header_block_is_incomplete() {
        let msg = b"INVITE sip:x SIP/2.0\r\nContent-Length: 4\r\n";
        assert_eq!(frame_len_of(msg), None);
    }

    #[test]
    fn headers_complete_but_body_short_is_incomplete() {
        let msg = b"INVITE sip:x SIP/2.0\r\nContent-Length: 10\r\n\r\nAB";
        // sip_frame_len reports the DECLARED total once headers are complete…
        assert_eq!(frame_len_of(msg), Some(msg.len() + 8));
        // …but the read path must hold (never emit, never over-drain) until
        // the body is complete. EOF with a short body → nothing emitted and
        // the buffered bytes untouched.
        let mut cur = std::io::Cursor::new(msg.to_vec());
        let mut buf = Vec::new();
        let emitted = {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(async { read_sip_frame(&mut cur, &mut buf).await.unwrap() })
        };
        assert_eq!(emitted, None, "incomplete frame must not be emitted");
        assert_eq!(buf, msg, "buffered bytes must be untouched");
    }

    #[test]
    fn no_content_length_means_headers_only() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\nVia: SIP/2.0/TCP a\r\n\r\n";
        assert_eq!(frame_len_of(msg), Some(msg.len()));
    }

    #[test]
    fn compact_content_length_header() {
        let msg = b"BYE sip:x SIP/2.0\r\nl: 2\r\n\r\nhi";
        assert_eq!(frame_len_of(msg), Some(msg.len()));
    }

    #[test]
    fn lf_only_line_endings_tolerated() {
        let msg = b"BYE sip:x SIP/2.0\nContent-Length: 0\n\n";
        assert_eq!(frame_len_of(msg), Some(msg.len()));
    }

    // ── WorkerRelay inert semantics (not installed → everything no-ops) ──

    #[tokio::test]
    async fn inert_relay_never_relays_or_reports() {
        let relay = WorkerRelay::new();
        assert!(relay.report_conn_up(ConnKind::Ws, "1.2.3.4:5".parse().unwrap(), "1.2.3.4:5".parse().unwrap()).is_none());
        assert_eq!(relay.foreign_dialog_owner("abc"), None);
        assert!(!relay.holds_real_conn(ConnKind::Ws, "1.2.3.4:5".parse().unwrap()));
        assert!(!relay.ensure_virtual_conn(ConnKind::Ws, "1.2.3.4:5".parse().unwrap()).await);
        let msg = rsipstack::sip::SipMessage::try_from(
            "BYE sip:x SIP/2.0\r\nCall-ID: abc\r\n\r\n".as_bytes(),
        )
        .unwrap();
        // Inert relay: maybe_relay_inbound must be a NO-OP — returning false
        // (process locally) is the only safe answer.
        assert!(!relay.maybe_relay_inbound(&msg, ConnKind::Ws, "1.2.3.4:5".parse().unwrap()));
        relay.report_conn_down(1);
        relay.unregister_real_write(ConnKind::Ws, "1.2.3.4:5".parse().unwrap());
        relay.handle_conn_closed(ConnKind::Ws, "1.2.3.4:5".parse().unwrap());
        relay.handle_relay_deliver(ConnKind::Ws, "1.2.3.4:5".parse().unwrap(), b"BYE").await;
        relay.apply_mirror_announce("abc".into(), 7);
        relay.apply_mirror_gone("abc");
        relay.apply_mirror_snapshot(vec![("abc".into(), 7)]);
    }

    // ── transport mapping ──

    #[test]
    fn conn_kind_transport_mapping_roundtrip() {
        use rsipstack::sip::transport::Transport;
        for t in [Transport::Tcp, Transport::Tls, Transport::Ws, Transport::Wss] {
            let k = conn_kind_of_transport(t).expect("mapped");
            assert_eq!(transport_of_conn_kind(k), t);
        }
        assert_eq!(conn_kind_of_transport(Transport::Udp), None);
    }

    // ── mirror semantics with an installed RelayLive ──

    fn installed_relay(generation: u32) -> Arc<WorkerRelay> {
        installed_relay_with_writers(generation, None)
    }

    /// `writers` = a REAL socketpair link's worker end, so relay frames can
    /// be asserted on the master-side handle's data_rx.
    fn installed_relay_with_writers(
        generation: u32,
        worker_link: Option<rustpbx_master_ipc::LinkHandle>,
    ) -> Arc<WorkerRelay> {
        let endpoint = rsipstack::transaction::endpoint::EndpointBuilder::new().build();
        let relay = WorkerRelay::new();
        let writers = Arc::new(LinkWriters::default());
        if let Some(link) = worker_link {
            writers.set(&link.control_tx, &link.data);
        }
        relay.install(Arc::new(RelayLive {
            generation,
            writers,
            endpoint: endpoint.inner.clone(),
            mirror: dashmap::DashMap::new(),
            virtual_conns: dashmap::DashMap::new(),
            real_writes: dashmap::DashMap::new(),
            conn_ids: AtomicU64::new(0),
            mirror_cap: MIRROR_CAP,
            pending_lookups: dashmap::DashMap::new(),
            lookup_nonce: AtomicU64::new(0),
            real_closes: dashmap::DashMap::new(),
        }));
        relay
    }

    #[tokio::test]
    async fn mirror_announce_routes_decision() {
        let relay = installed_relay(1);
        // Another generation owns the dialog → foreign.
        relay.apply_mirror_announce("call-a".into(), 2);
        assert_eq!(relay.foreign_dialog_owner("call-a"), Some(2));
        // Dual key: the announcement's full DialogId must ALSO resolve by its
        // bare wire Call-ID (the bridge scan looks up wire Call-IDs). Bare key
        // rule = everything before the FIRST '-' (same as the master's
        // wire_call_id).
        relay.apply_mirror_announce("dlg-abc-123-456".into(), 2);
        assert_eq!(relay.foreign_dialog_owner("dlg-abc-123-456"), Some(2));
        assert_eq!(relay.foreign_dialog_owner("dlg"), Some(2));
        // Own generation entries are NOT stored (decisions are local).
        relay.apply_mirror_announce("call-b".into(), 1);
        assert_eq!(relay.foreign_dialog_owner("call-b"), None);
        relay.apply_mirror_announce("dlg-b-x-y".into(), 1);
        assert_eq!(relay.foreign_dialog_owner("dlg-b"), None);
        // Snapshot replaces the mirror wholesale and drops self entries.
        relay.apply_mirror_snapshot(vec![
            ("call-a".into(), 2),
            ("call-mine".into(), 1),
        ]);
        assert_eq!(relay.foreign_dialog_owner("call-a"), Some(2));
        assert_eq!(relay.foreign_dialog_owner("call-mine"), None);
        // Gone removes both keys.
        // Realistic shape: wire Call-ID without inner '-', tags appended.
        let full = "8d9b263f@restsend.com-KtaL-VTy".to_string();
        relay.apply_mirror_announce(full.clone(), 2);
        assert_eq!(
            relay.foreign_dialog_owner("8d9b263f@restsend.com"),
            Some(2)
        );
        relay.apply_mirror_gone(&full);
        assert_eq!(relay.foreign_dialog_owner(full.as_str()), None);
        assert_eq!(relay.foreign_dialog_owner("8d9b263f@restsend.com"), None);
    }

    /// The relay scan decision must NEVER relay when the message carries no
    /// Call-ID (unparsable/garbage frames cannot be routed by the master).
    #[tokio::test]
    async fn message_without_call_id_is_never_relayed() {
        let relay = installed_relay(1);
        relay.apply_mirror_announce("any".into(), 2);
        let msg = rsipstack::sip::SipMessage::try_from(
            "OPTIONS sip:x SIP/2.0\r\nVia: SIP/2.0/UDP x\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert!(!relay.maybe_relay_inbound(&msg, ConnKind::Ws, "1.2.3.4:5".parse().unwrap()));
    }
    /// RelayToDialog frame from the worker lands on the master-side link,
    /// carrying the ConnKind + origin peer + SIP payload (Call-ID intact for
    /// the hub's DialogTable lookup).
    #[tokio::test]
    async fn relay_inbound_frame_reaches_master_link() {
        use rustpbx_master_ipc::{handle_for_end, socketpair_std};
        let (ca, cb) = socketpair_std().unwrap();
        let (da, db) = socketpair_std().unwrap();
        let mut master_end = handle_for_end(ca, da, 64).unwrap();
        let worker_end = handle_for_end(cb, db, 64).unwrap();
        let relay = installed_relay_with_writers(1, Some(worker_end));
        relay.apply_mirror_announce("call-x".into(), 2);

        let sip = "BYE sip:u@d SIP/2.0\r\nCall-ID: call-x\r\n\r\n";
        let msg = rsipstack::sip::SipMessage::try_from(sip.as_bytes()).unwrap();
        let peer: SocketAddr = "198.51.100.7:53000".parse().unwrap();
        assert!(
            relay.maybe_relay_inbound(&msg, ConnKind::Wss, peer),
            "foreign-owner message must be relayed"
        );
        let df = tokio::time::timeout(std::time::Duration::from_secs(5), master_end.data_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(df.kind, FrameKind::RelayToDialog);
        assert_eq!(rustpbx_master_ipc::frame::unpack_conn_kind(df.listener_id), Some(ConnKind::Wss));
        assert_eq!(df.addr, peer);
        let payload = String::from_utf8(df.datagram).unwrap();
        assert!(payload.contains("Call-ID: call-x"), "payload must carry the Call-ID for the hub scan: {payload}");
    }

    /// Conn-owner injection: a RelayDeliver for a REAL connection we hold is
    /// written onto the registered write path, byte-exact.
    #[tokio::test]
    async fn relay_deliver_injects_into_real_conn_write_path() {
        let relay = installed_relay(1);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let peer: SocketAddr = "198.51.100.9:53100".parse().unwrap();
        relay.register_real_write(ConnKind::SipTcp, peer, RealWrite::Bytes(tx));
        let sip = b"INVITE sip:u@d SIP/2.0\r\nCall-ID: out\r\n\r\n";
        relay.handle_relay_deliver(ConnKind::SipTcp, peer, sip).await;
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(got, sip);
    }

    /// Dialog-owner path: RelayDeliver without a real conn creates a virtual
    /// relay connection registered in the transport layer, findable by a
    /// lookup keyed on the peer SipAddr — the exact mechanism rsipstack's
    /// dead-affinity fallback relies on.
    #[tokio::test]
    async fn relay_deliver_creates_virtual_conn_findable_by_lookup() {
        let token = tokio_util::sync::CancellationToken::new();
        let endpoint = rsipstack::transaction::endpoint::EndpointBuilder::new()
            .with_transport_layer(rsipstack::transport::TransportLayer::new(token.clone()))
            .build();
        let relay = WorkerRelay::new();
        relay.install(Arc::new(RelayLive {
            generation: 1,
            writers: Arc::new(LinkWriters::default()),
            endpoint: endpoint.inner.clone(),
            mirror: dashmap::DashMap::new(),
            virtual_conns: dashmap::DashMap::new(),
            real_writes: dashmap::DashMap::new(),
            conn_ids: AtomicU64::new(0),
            mirror_cap: MIRROR_CAP,
            pending_lookups: dashmap::DashMap::new(),
            lookup_nonce: AtomicU64::new(0),
            real_closes: dashmap::DashMap::new(),
        }));
        let peer: SocketAddr = "198.51.100.11:53200".parse().unwrap();
        let sip = b"BYE sip:u@d SIP/2.0\r\nCall-ID: vb\r\n\r\n";
        relay.handle_relay_deliver(ConnKind::Ws, peer, sip).await;

        let tl = &endpoint.inner.transport_layer;
        assert_eq!(tl.connections_count(), 1, "virtual conn registered");
        let target = SipAddr {
            r#type: Some(rsipstack::sip::transport::Transport::Ws),
            addr: peer.into(),
        };
        let found = tl.lookup(&target, None).await;
        assert!(found.is_ok(), "lookup must find the virtual relay connection");
        assert_eq!(found.unwrap().0.get_remote_addr().unwrap().addr, target.addr);

        // ConnClosed tears the phantom flow down: registration + table entry
        // gone, so no later lookup can hand out the dead flow. (A follow-up
        // lookup on an empty table would attempt a client dial — browsers
        // don't listen — which is exactly the pre-relay behavior; the empty
        // table assertion is the meaningful check here.)
        relay.handle_conn_closed(ConnKind::Ws, peer);
        assert_eq!(tl.connections_count(), 0, "virtual conn removed");
    }
}
