//! Master access layer: owns public listeners, routes SIP, forwards fds.

use crate::sip_scan::extract_call_id;
use rustpbx_master_ipc::frame::FrameKind;
use rustpbx_master_ipc::ControlMessage;
use std::net::SocketAddr;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Source-address → generation affinity for datagrams whose Call-ID scan
/// fails (keepalives, exotic formats). Learned from Call-ID-bearing packets;
/// bounded by `max` + TTL sweep to stay leak-proof.
pub struct FlowTable {
    map: std::sync::Mutex<HashMap<SocketAddr, (u32, Instant)>>,
    max: usize,
    ttl: Duration,
}

impl FlowTable {
    pub fn new(max: usize, ttl: Duration) -> Self {
        Self { map: Mutex::new(HashMap::new()), max, ttl }
    }

    /// Record/refresh the owner for a source address.
    pub fn learn(&self, src: SocketAddr, generation: u32) {
        let mut map = self.lock();
        if map.len() >= self.max {
            let cutoff = Instant::now() - self.ttl;
            map.retain(|_, (_, seen)| *seen > cutoff);
            // Still full after the sweep → drop the oldest entry to stay bounded.
            if map.len() >= self.max && !map.is_empty() {
                if let Some(oldest) = map.iter().min_by_key(|(_, (_, seen))| *seen).map(|(k, _)| *k) {
                    map.remove(&oldest);
                }
            }
        }
        map.insert(src, (generation, Instant::now()));
    }

    /// Affinity lookup (refreshes the timestamp on hit).
    pub fn lookup(&self, src: &SocketAddr) -> Option<u32> {
        let mut map = self.lock();
        let cutoff = Instant::now() - self.ttl;
        map.retain(|_, (_, seen)| *seen > cutoff);
        if let Some((owner, seen)) = map.get_mut(src) {
            if *seen > cutoff {
                *seen = Instant::now();
                return Some(*owner);
            }
        }
        None
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SocketAddr, (u32, Instant)>> {
        self.map
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Default)]
pub struct AccessMetrics {
    pub udp_in: AtomicU64,
    pub udp_routed_dialog: AtomicU64,
    pub udp_routed_flow: AtomicU64,
    pub udp_no_route_drops: AtomicU64,
    pub udp_queue_full: AtomicU64,
    pub udp_flow_learned: AtomicU64,
    pub udp_flow_hits: AtomicU64,
    pub tcp_forwarded: AtomicU64,
    pub tcp_no_route_drops: AtomicU64,
    pub tcp_forward_errors: AtomicU64,
    pub tcp_sniffed: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlStreamKind {
    Http,
    SipTcp,
    SipTls,
    /// TLS the worker terminates (HTTPS/WSS) — fd forwarded untouched.
    HttpTls,
}

pub struct Access {
    pub core: Arc<crate::supervisor::SupCore>,
    pub metrics: Arc<AccessMetrics>,
    pub flows: Arc<FlowTable>,
}

impl Access {
    pub fn new(core: Arc<crate::supervisor::SupCore>) -> Self {
        Self {
            core,
            metrics: Arc::new(AccessMetrics::default()),
            flows: Arc::new(FlowTable::new(100_000, std::time::Duration::from_secs(60))),
        }
    }

    /// Bind the public UDP listeners.
    pub fn bind_udp(
        bind_ip: std::net::IpAddr,
        ports: &[u16],
    ) -> std::io::Result<Vec<(u16, Arc<tokio::net::UdpSocket>)>> {
        let mut out = Vec::new();
        for (idx, port) in ports.iter().enumerate() {
            let addr = SocketAddr::new(bind_ip, *port);
            let std_sock = std::net::UdpSocket::bind(addr)?;
            std_sock.set_nonblocking(true)?;
            let s = Arc::new(tokio::net::UdpSocket::from_std(std_sock)?);
            out.push((idx as u16, s));
        }
        Ok(out)
    }

    /// Spawn recv loops for each UDP listener: route by Call-ID → gen.
    pub fn spawn_udp_recvs(
        &self,
        sockets: Vec<(u16, Arc<tokio::net::UdpSocket>)>,
    ) {
        for (id, sock) in sockets {
            let access = Access {
                core: self.core.clone(),
                metrics: self.metrics.clone(),
                flows: self.flows.clone(),
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    match sock.recv_from(&mut buf).await {
                        Ok((n, src)) => {
                            access.metrics.udp_in.fetch_add(1, Ordering::Relaxed);
                            let cid = extract_call_id(&buf[..n]);
                            let is_bye = buf[..n].starts_with(b"BYE ");
                            let generation = match cid.as_deref() {
                                // Call-ID known → owner generation; learn the flow.
                                Some(cid) => access.core.route_gen(Some(cid)).await.inspect(|owner| {
                                    access.flows.learn(src, *owner);
                                    access.metrics.udp_flow_learned.fetch_add(1, Ordering::Relaxed);
                                }),
                                // No Call-ID → flow affinity, else the current gen.
                                None => match access.flows.lookup(&src) {
                                    Some(owner) => {
                                        access.metrics.udp_flow_hits.fetch_add(1, Ordering::Relaxed);
                                        Some(owner)
                                    }
                                    None => access.core.route_gen(None).await,
                                },
                            };
                            let Some(generation) = generation else {
                                if is_bye {
                                    eprintln!("[master-route] BYE NO-ROUTE src={src} cid={cid:?} flows={}", access.flows.lookup(&src).is_some());
                                }
                                access.metrics.udp_no_route_drops.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };
                            if is_bye {
                                eprintln!("[master-route] BYE -> gen{generation} cid={cid:?}");
                            }
                            let Some(link) = access.core.gen_link(generation).await else {
                                access.metrics.udp_no_route_drops.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };
                            if cid.is_some() {
                                access.metrics.udp_routed_dialog.fetch_add(1, Ordering::Relaxed);
                            } else {
                                access.metrics.udp_routed_flow.fetch_add(1, Ordering::Relaxed);
                            }
                            let frame = rustpbx_master_ipc::frame::encode_datagram(
                                FrameKind::UdpUp, id, &src, &buf[..n],
                            );
                            if !link.data.send(frame) {
                                // Per-worker queue full: drop + count (bounded
                                // memory; SIP retransmits recover lost UDP).
                                access.metrics.udp_queue_full.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        Err(_) => { tokio::time::sleep(std::time::Duration::from_millis(100)).await; }
                    }
                }
            });
        }
    }

    /// Spawn TCP/HTTP accept loops: forward fd to the routing-decided generation.
    ///
    /// Decision ladder (P0/P1):
    /// * `SipTcp`: sniff the first message's Call-ID (MSG_PEEK — the bytes
    ///   stay in the socket for the worker) → generation owning that dialog
    ///   → ConnTable peer affinity → current gen;
    /// * `SipTls`: ConnTable peer affinity → current gen (TLS hides the
    ///   first message; peer affinity is what survives reconnects);
    /// * `Http`/`HttpTls`: current gen — affinity would stick browser REST
    ///   traffic to a draining generation; ws continuity is the relay's job;
    /// * when the ladder has no answer AND the flip window is open
    ///   (`current = None`): fall back to the most-active generation
    ///   (`[master] tcp_fallback`, default on) instead of dropping.
    pub fn spawn_tcp_forward(
        &self,
        addr: SocketAddr,
        kind: crate::access::ControlStreamKind,
    ) -> std::io::Result<()> {
        let std_listener = std::net::TcpListener::bind(addr)?;
        std_listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(std_listener)?;
        let core = self.core.clone();
        let metrics = self.metrics.clone();
        let sniff_enabled = core.cfg.master.tcp_callid_sniff();
        let fallback_enabled = core.cfg.master.tcp_fallback();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, peer)) => {
                        // ── routing ladder ──
                        let mut generation = match kind {
                            crate::access::ControlStreamKind::SipTcp if sniff_enabled => {
                                match sniff_call_id(&sock).await {
                                    Some(cid) => {
                                        metrics.tcp_sniffed.fetch_add(1, Ordering::Relaxed);
                                        core.route_gen(Some(&cid)).await
                                    }
                                    None => None,
                                }
                            }
                            crate::access::ControlStreamKind::SipTls => conn_kind_for(kind)
                                .and_then(|k| core.conns.owner(k, &peer)),
                            _ => None,
                        };
                        // Current generation (None during the flip window).
                        if generation.is_none() {
                            generation = core.route_gen(None).await;
                        }
                        // Flip window fallback: most-active generation.
                        if generation.is_none() && fallback_enabled {
                            generation = core.fallback_gen().await;
                        }
                        let Some(generation) = generation else {
                            metrics.tcp_no_route_drops.fetch_add(1, Ordering::Relaxed);
                            drop(sock);
                            continue;
                        };
                        let Some(gen_link) = core.gen_link(generation).await else {
                            metrics.tcp_no_route_drops.fetch_add(1, Ordering::Relaxed);
                            drop(sock);
                            continue;
                        };
                        let local = sock.local_addr().unwrap_or(addr);
                        let Ok(std_sock) = sock.into_std() else { continue };
                        let _ = std_sock.set_nonblocking(false);
                        let sent = gen_link.control.send_with_fd(
                            ControlMessage::TcpConn {
                                conn_id: next_conn_id(),
                                kind: match kind {
                                    crate::access::ControlStreamKind::Http => rustpbx_master_ipc::StreamKind::Http,
                                    crate::access::ControlStreamKind::SipTcp => rustpbx_master_ipc::StreamKind::SipTcp,
                                    crate::access::ControlStreamKind::SipTls => rustpbx_master_ipc::StreamKind::SipTls,
                                    crate::access::ControlStreamKind::HttpTls => rustpbx_master_ipc::StreamKind::HttpTls,
                                },
                                local,
                                peer,
                            },
                            std_sock.as_raw_fd(),
                        );
                        if sent.is_ok() {
                            metrics.tcp_forwarded.fetch_add(1, Ordering::Relaxed);
                        } else {
                            metrics.tcp_forward_errors.fetch_add(1, Ordering::Relaxed);
                        }
                        drop(std_sock);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    Err(_) => { tokio::time::sleep(std::time::Duration::from_millis(100)).await; }
                }
            }
        });
        Ok(())
    }
}

/// ControlStreamKind → the ConnKind its worker-reported connections use.
fn conn_kind_for(kind: ControlStreamKind) -> Option<rustpbx_master_ipc::ConnKind> {
    match kind {
        ControlStreamKind::SipTcp => Some(rustpbx_master_ipc::ConnKind::SipTcp),
        ControlStreamKind::SipTls => Some(rustpbx_master_ipc::ConnKind::SipTls),
        // Worker ws/wss conns report as ConnKind::Ws/Wss; an Http/HttpTls
        // accept never matches them (affinity is not applied to HTTP).
        _ => None,
    }
}

/// Peek the first SIP message off a freshly accepted TCP connection and
/// extract its Call-ID. `MSG_PEEK` semantics: the bytes stay in the kernel
/// buffer for the worker. `peek` re-returns the whole buffer each call, so
/// the window grows until the message's headers are complete. Bounded
/// (64 KB / 2 s) — a silent peer still gets forwarded (down the ladder)
/// instead of stalling the accept loop.
async fn sniff_call_id(sock: &tokio::net::TcpStream) -> Option<String> {
    const MAX_PEEK: usize = 64 * 1024;
    const STEP: std::time::Duration = std::time::Duration::from_millis(5);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut size = 2048usize;
    loop {
        let mut buf = vec![0u8; size];
        let n = tokio::time::timeout_at(deadline, sock.peek(&mut buf)).await.ok()?.ok()?;
        if n == 0 {
            return None; // peer closed before speaking
        }
        if let Some(cid) = crate::sip_scan::extract_call_id(&buf[..n]) {
            return Some(cid);
        }
        if n >= MAX_PEEK || size >= MAX_PEEK {
            return None;
        }
        // Either the buffer is full of a partial message (grow) or we've
        // drained what's buffered and must wait for more segments.
        size = (size * 2).min(MAX_PEEK);
        tokio::time::sleep(STEP).await;
    }
}

pub fn next_conn_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod flow_table_tests {
    use super::*;

    #[test]
    fn learn_then_affinity_hit() {
        let t = FlowTable::new(10, Duration::from_secs(60));
        let src: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        assert_eq!(t.lookup(&src), None);
        t.learn(src, 7);
        assert_eq!(t.lookup(&src), Some(7));
        assert_eq!(t.lookup(&src), Some(7)); // refresh keeps it alive
    }

    #[test]
    fn ttl_expiry_clears_entry() {
        let t = FlowTable::new(10, Duration::from_millis(0));
        let src: SocketAddr = "10.0.0.2:5060".parse().unwrap();
        t.learn(src, 1);
        // ttl=0 → the entry is expired on the next lookup.
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(t.lookup(&src), None);
    }

    #[test]
    fn cap_sweeps_and_stays_bounded() {
        let t = FlowTable::new(3, Duration::from_secs(60));
        for i in 0..5u16 {
            let src: SocketAddr = format!("10.0.0.{i}:5060").parse().unwrap();
            t.learn(src, i as u32);
        }
        // At most `max` live entries after the sweep+evict.
        assert!(t.lookup(&"10.0.0.0:5060".parse().unwrap()).is_none()
            || t.lookup(&"10.0.0.4:5060".parse().unwrap()).is_some());
        let inner = t.lock().len();
        assert!(inner <= 3, "flow table exceeded cap: {inner}");
    }
}
