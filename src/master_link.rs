//! Worker-side master link for `--master <sock>` mode.
//!
//! In worker mode the process binds no public ports. The master:
//! 1. Forwards UDP datagrams as `FrameKind::UdpUp` frames on the data channel
//! 2. Forwards TCP/HTTP connections as `TcpConn` + SCM_RIGHTS fd on the control channel
//!
//! The worker registers SIP transports dynamically via `add_connection`
//! (auto-starts serve loop + endpoint dispatch), same pattern as `ws.rs`.

use rustpbx_master_ipc::{
    ControlMessage, ControlSender, DataWriter, FrameKind, LinkHandle, worker_connect,
};
use rsipstack::transport::{
    channel::ChannelConnection,
    tcp::TcpConnection,
    tls::TlsConnection,
    SipAddr, SipConnection, TransportEvent,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

// ── public types ──

pub trait DialogIdSource: Send + Sync + 'static {
    fn live_dialog_ids(&self) -> Vec<String>;
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
) {
    let dialogs: Arc<dyn DialogIdSource> = Arc::new(SipServerDialogs {
        inner: state.sip_server().inner.clone(),
    });
    let tls_acceptor = build_tls_acceptor(&state);
    let relays: Arc<dashmap::DashMap<u16, tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>>> =
        Arc::new(dashmap::DashMap::new());
    let writers = Arc::new(LinkWriters::default());

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
async fn run_link(
    link: LinkHandle,
    generation: u32,
    writers: Arc<LinkWriters>,
    relays: Arc<dashmap::DashMap<u16, tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>>>,
    state: crate::app::AppState,
    router: axum::Router,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
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
    let control_task = tokio::spawn(async move {
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
                            serve_sip_tcp_fd(&state_ctrl.sip_server().inner.endpoint.inner.transport_layer, fd, local);
                        }
                        rustpbx_master_ipc::StreamKind::Http => {
                            serve_http(fd, peer, router_ctrl.clone());
                        }
                        rustpbx_master_ipc::StreamKind::SipTls => {
                            if let Some(acceptor) = &tls_acceptor {
                                let stream = std::net::TcpStream::from(fd);
                                let _ = stream.set_nonblocking(true);
                                let acceptor = acceptor.clone();
                                let state_tls = state_ctrl.clone();
                                tokio::spawn(async move {
                                    let Ok(stream) = tokio::net::TcpStream::from_std(stream) else { return };
                                    match acceptor.accept(stream).await {
                                        Ok(tls) => {
                                            let sip_local = SipAddr {
                                                r#type: Some(rsipstack::sip::transport::Transport::Tls),
                                                addr: local.into(),
                                            };
                                            match TlsConnection::from_server_stream(tls, sip_local.clone(), sip_local, None).await {
                                                Ok(conn) => state_tls.sip_server().inner.endpoint.inner.transport_layer.add_connection(SipConnection::Tls(conn)),
                                                Err(e) => tracing::warn!("TlsConnection: {e}"),
                                            }
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
                ControlMessage::Drain { .. } => {
                    tracing::info!("master requested drain");
                    // Same semantics as SIGTERM drain: reject new calls, wait
                    // for active dialogs to finish — but do NOT cancel the
                    // runtime token; the master sends Stop when it has seen
                    // Drained. Runs inline: while draining we deliberately
                    // stop consuming other control messages.
                    let first = crate::shutdown::initiate();
                    let timeout = state_ctrl.config().graceful_shutdown_timeout();
                    let sip = state_ctrl.sip_server();
                    if first {
                        let active_calls = sip.inner.active_call_registry.count();
                        tracing::info!(
                            ?timeout,
                            active_calls,
                            "draining: rejecting new calls, waiting for dialogs to end"
                        );
                    }
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
                    let _ = writers.control().map(|c| c.send(ControlMessage::Drained));
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

    // ── UDP data pump (per link) ──
    let relays_dp = relays.clone();
    let mut data_rx = link.data_rx;
    let data_task = tokio::spawn(async move {
        while let Some(df) = data_rx.recv().await {
            if df.kind != FrameKind::UdpUp { continue; }
            if let Some(tx) = relays_dp.get(&df.listener_id) {
                let _ = tx.send((df.datagram, df.addr));
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

/// Serve a master-forwarded TCP connection as SIP TCP.
fn serve_sip_tcp_fd(
    tl: &rsipstack::transport::TransportLayer,
    fd: std::os::fd::OwnedFd,
    local: SocketAddr,
) {
    let stream = std::net::TcpStream::from(fd);
    let _ = stream.set_nonblocking(true);
    let Ok(stream) = tokio::net::TcpStream::from_std(stream) else { return };
    let addr = SipAddr {
        r#type: Some(rsipstack::sip::transport::Transport::Tcp),
        addr: local.into(),
    };
    match TcpConnection::from_stream(stream, addr, None) {
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
