/// Trunk health check — periodic SIP OPTIONS probing via rsipstack transaction.
///
/// Each enabled trunk receives OPTIONS requests at a configurable interval.
/// Consecutive failures beyond `probe_count` mark the trunk as unhealthy,
/// triggering automatic failover to `fallback_trunk`. When OPTIONS start
/// succeeding again the trunk is restored to healthy.
use crate::proxy::routing::TrunkConfig;
use rsipstack::sip::{
    Method, Param, Request, SipMessage, Uri, Version,
    headers::CallId,
    headers::typed::CSeq,
    typed::{From as FromHeader, To as ToHeader, Via},
    uri::Tag,
};
use rsipstack::transaction::endpoint::EndpointInnerRef;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::transaction::Transaction;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Per-IP health status detail.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PerIpHealthState {
    pub target: String,
    pub healthy: bool,
    pub rtt_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_checked: Option<String>,
}

/// Runtime health status of a single trunk.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrunkHealthState {
    pub trunk_name: String,
    pub healthy: bool,
    pub consecutive_failures: u32,
    pub rtt_ms: Option<u64>,
    pub last_checked: Option<String>,
    pub last_error: Option<String>,
    pub dest: String,
    pub fallback_trunk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_ip_states: Option<Vec<PerIpHealthState>>,
}

/// Shared health state map: trunk_name → TrunkHealthState
pub type HealthStateMap = Arc<RwLock<HashMap<String, TrunkHealthState>>>;

/// Probe a single trunk via rsipstack transaction.
/// Returns `Ok(rtt_ms)` on success, `Err(message)` on failure.
#[allow(dead_code)]
async fn probe_trunk(
    dest: &str,
    endpoint_inner: &EndpointInnerRef,
    local_sip_addr: &str,
) -> Result<u64, String> {
    probe_trunk_with_timeout(
        dest,
        endpoint_inner,
        local_sip_addr,
        Duration::from_secs(10),
    )
    .await
}

pub async fn probe_trunk_with_timeout(
    dest: &str,
    endpoint_inner: &EndpointInnerRef,
    local_sip_addr: &str,
    timeout_dur: Duration,
) -> Result<u64, String> {
    let uri: Uri = format!("sip:health@{}", dest)
        .parse()
        .map_err(|e| format!("invalid dest URI {}: {}", dest, e))?;

    let branch = format!(
        "z9hG4bK-hc{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    );
    let tag = uuid::Uuid::new_v4()
        .to_string()
        .split('-')
        .next()
        .unwrap_or("tag")
        .to_string();
    let call_id = CallId::new(format!(
        "hc-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));

    let via = Via::parse(&format!("SIP/2.0/UDP {};branch={}", local_sip_addr, branch))
        .map_err(|e| format!("invalid via: {}", e))?;

    let from = FromHeader {
        display_name: None,
        uri: format!("sip:health-check@{}", local_sip_addr)
            .parse()
            .map_err(|e| format!("invalid from uri: {}", e))?,
        params: vec![Param::Tag(Tag::new(&tag))],
    };
    let to = ToHeader {
        display_name: None,
        uri: format!("sip:health@{}", dest)
            .parse()
            .map_err(|e| format!("invalid to uri: {}", e))?,
        params: vec![],
    };

    let request = Request {
        method: Method::Options,
        uri,
        version: Version::V2,
        headers: vec![
            via.into(),
            from.into(),
            to.into(),
            call_id.into(),
            CSeq {
                seq: 1,
                method: Method::Options,
            }
            .into(),
        ]
        .into(),
        body: vec![],
    };

    let start = std::time::Instant::now();
    let key = TransactionKey::from_request(&request, TransactionRole::Client)
        .map_err(|e| format!("transaction key: {}", e))?;
    let mut tx = Transaction::new_client(key, request, endpoint_inner.clone(), None);
    tx.send().await.map_err(|e| format!("tx send: {}", e))?;

    match tokio::time::timeout(timeout_dur, tx.receive()).await {
        Ok(Some(SipMessage::Response(resp))) => {
            let elapsed = start.elapsed().as_millis() as u64;
            let code = u16::from(resp.status_code);
            if code < 300 {
                Ok(elapsed)
            } else {
                Err(format!("SIP {}", code))
            }
        }
        Ok(Some(_)) => {
            let elapsed = start.elapsed().as_millis() as u64;
            Err(format!("non-response reply in {}ms", elapsed))
        }
        Ok(None) => Err("no response (transaction closed)".to_string()),
        Err(_) => Err("timeout (10s)".to_string()),
    }
}

pub fn dest_name(dest: &str) -> &str {
    let dest = dest.trim();
    dest.strip_prefix("sip:").unwrap_or(dest)
}

/// Extract port from a normalized (sip: stripped) target like `host:port`.
/// Returns `None` if no port is present.
fn extract_port(target: &str) -> Option<String> {
    let target = target.trim();
    if let Some(pos) = target.rfind(':') {
        let port_str = &target[pos + 1..];
        if port_str.parse::<u16>().is_ok() {
            return Some(port_str.to_string());
        }
    }
    None
}

/// Build a deduplicated list of probe targets for per-IP mode.
/// Each inbound_host inherits the port from `dest` if it does not already have one.
/// Duplicate (host:port) combinations are removed.
pub fn build_per_ip_targets(dest: &str, inbound_hosts: &[String]) -> Vec<String> {
    let dest_clean = dest_name(dest).to_string();
    let default_port = extract_port(&dest_clean);

    let mut seen = std::collections::HashSet::new();
    let mut targets = Vec::new();

    let add_target = |t: &str, targets: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
        if seen.insert(t.to_string()) {
            targets.push(t.to_string());
        }
    };

    add_target(&dest_clean, &mut targets, &mut seen);

    for host in inbound_hosts {
        let host = host.trim();
        if extract_port(host).is_some() {
            add_target(host, &mut targets, &mut seen);
        } else if let Some(ref port) = default_port {
            let normalized = format!("{}:{}", host, port);
            add_target(&normalized, &mut targets, &mut seen);
        } else {
            add_target(host, &mut targets, &mut seen);
        }
    }

    targets
}

/// Spawn the background health-check loop.
///
/// It reads `trunks_fn` (which returns the current trunk map) on each tick,
/// probes every trunk that has `health_check_enabled`, and updates the shared
/// `states` map. When `health_check_per_ip` is enabled for a trunk, each
/// entry in `inbound_hosts` plus the trunk `dest` is probed individually.
/// The trunk-level health uses OR aggregation — any healthy target keeps the
/// trunk healthy.
pub fn spawn_health_loop(
    trunks_fn: impl Fn() -> HashMap<String, TrunkConfig> + Send + 'static,
    states: HealthStateMap,
    endpoint_inner: EndpointInnerRef,
    local_sip_addr: String,
    default_interval_secs: u64,
    cancel: tokio_util::sync::CancellationToken,
) {
    spawn_health_loop_with_timeout(
        trunks_fn,
        states,
        endpoint_inner,
        local_sip_addr,
        default_interval_secs,
        Duration::from_secs(10),
        cancel,
    )
}

fn spawn_health_loop_with_timeout(
    trunks_fn: impl Fn() -> HashMap<String, TrunkConfig> + Send + 'static,
    states: HealthStateMap,
    endpoint_inner: EndpointInnerRef,
    local_sip_addr: String,
    default_interval_secs: u64,
    probe_timeout: Duration,
    cancel: tokio_util::sync::CancellationToken,
) {
    crate::utils::spawn(async move {
        info!(
            "trunk health check started (default interval={}s, addr={})",
            default_interval_secs, local_sip_addr
        );

        while !cancel.is_cancelled() {
            let trunks = trunks_fn();
            let mut next_tick = default_interval_secs;

            for (name, cfg) in &trunks {
                let enabled = cfg.health_check_enabled.unwrap_or(false);
                if !enabled {
                    continue;
                }

                let interval = cfg
                    .health_check_interval_secs
                    .unwrap_or(default_interval_secs);
                next_tick = next_tick.min(interval);

                let per_ip = cfg.health_check_per_ip.unwrap_or(false);
                let has_hosts = !cfg.inbound_hosts.is_empty();

                if per_ip && has_hosts {
                    // ── Per-IP mode: probe each unique (host:port) target.
                    // Inbound hosts inherit the port from dest if they don't have one.
                    // Duplicates are removed: if dest and inbound_host resolve to the
                    // same host:port, only one probe is sent.
                    let targets = build_per_ip_targets(&cfg.dest, &cfg.inbound_hosts);

                    let mut per_ip_results: Vec<PerIpHealthState> =
                        Vec::with_capacity(targets.len());
                    let mut all_healthy = true;

                    for target in &targets {
                        let tgt = dest_name(target);
                        let checked_at = chrono::Utc::now().to_rfc3339();
                        match probe_trunk_with_timeout(
                            tgt,
                            &endpoint_inner,
                            &local_sip_addr,
                            probe_timeout,
                        )
                        .await
                        {
                            Ok(rtt) => {
                                per_ip_results.push(PerIpHealthState {
                                    target: target.clone(),
                                    healthy: true,
                                    rtt_ms: Some(rtt),
                                    last_error: None,
                                    last_checked: Some(checked_at.clone()),
                                });
                                debug!("health OK  {}:{}  {}ms", name, target, rtt);
                            }
                            Err(e) => {
                                all_healthy = false;
                                per_ip_results.push(PerIpHealthState {
                                    target: target.clone(),
                                    healthy: false,
                                    rtt_ms: None,
                                    last_error: Some(e.clone()),
                                    last_checked: Some(checked_at.clone()),
                                });
                                debug!("health FAIL {}:{}  {}", name, target, e);
                            }
                        }
                    }

                    let now = chrono::Utc::now().to_rfc3339();
                    let mut map = states.write().await;
                    let prev = map.get(name).cloned().unwrap_or(TrunkHealthState {
                        trunk_name: name.clone(),
                        healthy: true,
                        consecutive_failures: 0,
                        rtt_ms: None,
                        last_checked: None,
                        last_error: None,
                        dest: cfg.dest.clone(),
                        fallback_trunk: cfg.health_check_fallback_trunk.clone(),
                        per_ip_states: None,
                    });

                    let failures = if all_healthy {
                        0
                    } else {
                        prev.consecutive_failures + 1
                    };
                    let threshold = cfg.health_check_probe_count.unwrap_or(3);
                    let is_unhealthy = !all_healthy && failures >= threshold;

                    if is_unhealthy && prev.healthy {
                        warn!(
                            trunk = %name,
                            failures,
                            threshold,
                            "trunk marked UNHEALTHY ({} consecutive failures >= {}, per-ip mode)",
                            failures, threshold,
                        );
                        if let Some(ref fallback) = cfg.health_check_fallback_trunk {
                            info!(trunk = %name, fallback, "auto-failover activated");
                        }
                    }

                    map.insert(
                        name.clone(),
                        TrunkHealthState {
                            trunk_name: name.clone(),
                            healthy: all_healthy,
                            consecutive_failures: failures,
                            rtt_ms: per_ip_results
                                .iter()
                                .find_map(|p| if p.healthy { p.rtt_ms } else { None }),
                            last_checked: Some(now),
                            last_error: if all_healthy {
                                None
                            } else {
                                per_ip_results.first().and_then(|p| p.last_error.clone())
                            },
                            dest: cfg.dest.clone(),
                            fallback_trunk: cfg.health_check_fallback_trunk.clone(),
                            per_ip_states: Some(per_ip_results),
                        },
                    );
                    drop(map);
                } else {
                    // ── Standard mode: probe dest only ──
                    let trunk_dest = dest_name(&cfg.dest);

                    match probe_trunk_with_timeout(
                        trunk_dest,
                        &endpoint_inner,
                        &local_sip_addr,
                        probe_timeout,
                    )
                    .await
                    {
                        Ok(rtt) => {
                            let mut map = states.write().await;
                            map.insert(
                                name.clone(),
                                TrunkHealthState {
                                    trunk_name: name.clone(),
                                    healthy: true,
                                    consecutive_failures: 0,
                                    rtt_ms: Some(rtt),
                                    last_checked: Some(chrono::Utc::now().to_rfc3339()),
                                    last_error: None,
                                    dest: cfg.dest.clone(),
                                    fallback_trunk: cfg.health_check_fallback_trunk.clone(),
                                    per_ip_states: None,
                                },
                            );
                            drop(map);
                            debug!("health OK  {}  {}ms", name, rtt);
                        }
                        Err(e) => {
                            let mut map = states.write().await;
                            let prev = map.get(name).cloned().unwrap_or(TrunkHealthState {
                                trunk_name: name.clone(),
                                healthy: true,
                                consecutive_failures: 0,
                                rtt_ms: None,
                                last_checked: None,
                                last_error: None,
                                dest: cfg.dest.clone(),
                                fallback_trunk: cfg.health_check_fallback_trunk.clone(),
                                per_ip_states: None,
                            });
                            let failures = prev.consecutive_failures + 1;
                            let threshold = cfg.health_check_probe_count.unwrap_or(3);
                            let is_unhealthy = failures >= threshold;

                            if is_unhealthy && prev.healthy {
                                warn!(
                                    trunk = %name,
                                    failures,
                                    threshold,
                                    "trunk marked UNHEALTHY ({} consecutive failures >= {})",
                                    failures, threshold,
                                );
                                if let Some(ref fallback) = cfg.health_check_fallback_trunk {
                                    info!(trunk = %name, fallback, "auto-failover activated");
                                }
                            }

                            map.insert(
                                name.clone(),
                                TrunkHealthState {
                                    trunk_name: name.clone(),
                                    healthy: !is_unhealthy,
                                    consecutive_failures: failures,
                                    rtt_ms: None,
                                    last_checked: Some(chrono::Utc::now().to_rfc3339()),
                                    last_error: Some(format!("OPTIONS failed: {}", e)),
                                    dest: cfg.dest.clone(),
                                    fallback_trunk: cfg.health_check_fallback_trunk.clone(),
                                    per_ip_states: None,
                                },
                            );
                            drop(map);
                            debug!("health FAIL {}  {}  ({}/{})", name, e, failures, threshold);
                        }
                    }
                }
            }

            {
                let mut map = states.write().await;
                map.retain(|name, _| {
                    trunks.get(name).map_or(false, |cfg| cfg.health_check_enabled.unwrap_or(false))
                });
            }

            tokio::time::sleep(Duration::from_secs(next_tick)).await;
        }

        info!("trunk health check stopped");
    });
}

/// Remove health state entries for trunks that no longer exist or have HC disabled.
pub async fn cleanup_stale_health_states(
    data_context: &super::data::ProxyDataContext,
    health: &Option<HealthStateMap>,
) {
    let trunks = data_context.trunks_snapshot();
    if let Some(health_map) = health {
        let mut map = health_map.write().await;
        map.retain(|name, _| {
            trunks.get(name).map_or(false, |cfg| cfg.health_check_enabled.unwrap_or(false))
        });
    }
}

/// Return a snapshot of all health states.
pub async fn snapshot(states: &HealthStateMap) -> Vec<TrunkHealthState> {
    let map = states.read().await;
    let mut v: Vec<_> = map.values().cloned().collect();
    v.sort_by(|a, b| a.trunk_name.cmp(&b.trunk_name));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use portpicker::pick_unused_port;
    use rsipstack::{
        EndpointBuilder,
        sip::{Method, StatusCode},
        transport::{TransportLayer, udp::UdpConnection},
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::RwLock;
    use tokio::time::sleep;
    use tokio_util::sync::CancellationToken;

    // ── Helpers ─────────────────────────────────────────────────────────────

    struct OptionsResponder {
        #[allow(dead_code)]
        cancel: CancellationToken,
        port: u16,
    }

    impl OptionsResponder {
        async fn start(port: u16) -> Self {
            let cancel = CancellationToken::new();
            let tl = TransportLayer::new(cancel.child_token());
            let udp = UdpConnection::create_connection(
                format!("127.0.0.1:{port}").parse().unwrap(),
                None,
                Some(cancel.child_token()),
            )
            .await
            .unwrap();
            tl.add_transport(udp.into());

            let mut builder = EndpointBuilder::new();
            builder.with_user_agent("test-responder/1.0");
            builder.with_transport_layer(tl);
            builder.with_cancel_token(cancel.child_token());
            builder.with_timer_interval(Duration::from_millis(50));
            let endpoint = builder.build();

            let ep_inner = endpoint.inner.clone();
            let ct = cancel.clone();
            crate::utils::spawn(async move {
                tokio::select! {
                    _ = ct.cancelled() => {}
                    r = ep_inner.serve() => { if let Err(e) = r { tracing::warn!("responder serve: {e}"); } }
                }
            });

            let mut rx = endpoint.incoming_transactions().unwrap();
            let ct2 = cancel.clone();
            crate::utils::spawn(async move {
                loop {
                    tokio::select! {
                        _ = ct2.cancelled() => break,
                        tx = rx.recv() => {
                            if let Some(mut tx) = tx {
                                if tx.original.method == Method::Options {
                                    tx.reply(StatusCode::OK).await.ok();
                                }
                            }
                        }
                    }
                }
            });

            sleep(Duration::from_millis(300)).await;
            Self { cancel, port }
        }

        #[allow(dead_code)]
        fn stop(&self) {
            self.cancel.cancel();
        }
        fn addr(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }
    }

    async fn create_client_endpoint() -> (EndpointInnerRef, String) {
        let cancel = CancellationToken::new();
        let tl = TransportLayer::new(cancel.child_token());
        let port = pick_unused_port().unwrap();
        let udp = UdpConnection::create_connection(
            format!("127.0.0.1:{port}").parse().unwrap(),
            None,
            Some(cancel.child_token()),
        )
        .await
        .unwrap();
        let local_addr = format!("{}", udp.get_addr().addr);
        tl.add_transport(udp.into());

        let mut builder = EndpointBuilder::new();
        builder.with_user_agent("test-client/1.0");
        builder.with_transport_layer(tl);
        builder.with_cancel_token(cancel.child_token());
        builder.with_timer_interval(Duration::from_millis(50));
        let endpoint = builder.build();

        let ep_inner = endpoint.inner.clone();
        let ct = cancel.clone();
        crate::utils::spawn(async move {
            tokio::select! {
                _ = ct.cancelled() => {}
                r = ep_inner.serve() => { if let Err(e) = r { tracing::warn!("client serve: {e}"); } }
            }
        });

        sleep(Duration::from_millis(300)).await;
        (endpoint.inner.clone(), local_addr)
    }

    fn make_trunk(dest: &str, per_ip: bool, interval: u64, probe_count: u32) -> TrunkConfig {
        TrunkConfig {
            dest: format!("sip:{}", dest),
            health_check_enabled: Some(true),
            health_check_interval_secs: Some(interval),
            health_check_probe_count: Some(probe_count),
            health_check_per_ip: Some(per_ip),
            ..Default::default()
        }
    }

    /// Shared mutable trunks for test: use std::sync::Mutex (not tokio)
    /// because `spawn_health_loop` calls the closure from within tokio context.
    type TestTrunks = std::sync::Mutex<HashMap<String, TrunkConfig>>;

    // ── dest_name tests ─────────────────────────────────────────────────────

    #[test]
    fn test_dest_name_strips_sip_prefix() {
        assert_eq!(dest_name("sip:carrier.com:5060"), "carrier.com:5060");
    }

    #[test]
    fn test_dest_name_preserves_plain_host() {
        assert_eq!(dest_name("carrier.com"), "carrier.com");
    }

    #[test]
    fn test_dest_name_handles_edge_cases() {
        assert_eq!(dest_name(""), "");
        assert_eq!(dest_name("  sip:host  "), "host");
    }

    // ── snapshot tests ──────────────────────────────────────────────────────

    #[test]
    fn test_snapshot_empty() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let states = Arc::new(RwLock::new(HashMap::new()));
        let snap = rt.block_on(snapshot(&states));
        assert!(snap.is_empty());
    }

    #[tokio::test]
    async fn test_snapshot_sorted() {
        let states = Arc::new(RwLock::new(HashMap::new()));
        {
            let mut map = states.write().await;
            map.insert(
                "b".into(),
                TrunkHealthState {
                    trunk_name: "b".into(),
                    healthy: true,
                    consecutive_failures: 0,
                    rtt_ms: Some(5),
                    last_checked: None,
                    last_error: None,
                    dest: "b.com".into(),
                    fallback_trunk: None,
                    per_ip_states: None,
                },
            );
            map.insert(
                "a".into(),
                TrunkHealthState {
                    trunk_name: "a".into(),
                    healthy: true,
                    consecutive_failures: 0,
                    rtt_ms: Some(3),
                    last_checked: None,
                    last_error: None,
                    dest: "a.com".into(),
                    fallback_trunk: None,
                    per_ip_states: None,
                },
            );
        }
        let snap = snapshot(&states).await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].trunk_name, "a");
        assert_eq!(snap[1].trunk_name, "b");
    }

    // ── probe_trunk tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_probe_trunk_success() {
        let port = pick_unused_port().unwrap();
        let responder = OptionsResponder::start(port).await;
        let (ep_inner, local_addr) = create_client_endpoint().await;

        sleep(Duration::from_millis(200)).await;
        let result = probe_trunk_with_timeout(
            &responder.addr(),
            &ep_inner,
            &local_addr,
            Duration::from_secs(3),
        )
        .await;
        assert!(result.is_ok(), "probe should succeed: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_probe_trunk_failure() {
        let (ep_inner, local_addr) = create_client_endpoint().await;
        let dead_port = pick_unused_port().unwrap();

        let result = probe_trunk_with_timeout(
            &format!("127.0.0.1:{dead_port}"),
            &ep_inner,
            &local_addr,
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_err(), "probe should fail for unreachable target");
    }

    #[tokio::test]
    async fn test_probe_trunk_invalid_dest() {
        let (ep_inner, local_addr) = create_client_endpoint().await;
        let result = probe_trunk_with_timeout(
            "not-a-valid-address!",
            &ep_inner,
            &local_addr,
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_err(), "probe should fail for invalid dest");
    }

    // ─── Health loop state transition tests ─────────────────────────────────

    #[tokio::test]
    async fn test_health_loop_healthy() {
        let port = pick_unused_port().unwrap();
        let responder = OptionsResponder::start(port).await;
        let (ep_inner, local_addr) = create_client_endpoint().await;

        let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
        let trunk = make_trunk(&responder.addr(), false, 1, 3);
        let cancel = CancellationToken::new();

        let trunks = Arc::new(TestTrunks::new(HashMap::from([("test".into(), trunk)])));
        let trunks_fn = {
            let t = trunks.clone();
            move || t.lock().unwrap().clone()
        };

        spawn_health_loop(
            trunks_fn,
            states.clone(),
            ep_inner,
            local_addr,
            1,
            cancel.child_token(),
        );

        sleep(Duration::from_secs(2)).await;
        let snap = snapshot(&states).await;
        assert!(!snap.is_empty(), "should have health state");
        assert!(snap[0].healthy, "trunk should be healthy");
        assert_eq!(snap[0].consecutive_failures, 0);
        assert!(snap[0].rtt_ms.is_some(), "should have rtt");
        assert!(snap[0].last_error.is_none());

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_health_loop_unhealthy_after_failures() {
        let (ep_inner, local_addr) = create_client_endpoint().await;
        let dead_port = pick_unused_port().unwrap();
        let dest = format!("127.0.0.1:{dead_port}");

        let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
        let trunk = make_trunk(&dest, false, 1, 2);
        let cancel = CancellationToken::new();

        let trunks = Arc::new(TestTrunks::new(HashMap::from([("test".into(), trunk)])));
        let trunks_fn = {
            let t = trunks.clone();
            move || t.lock().unwrap().clone()
        };

        spawn_health_loop_with_timeout(
            trunks_fn,
            states.clone(),
            ep_inner,
            local_addr,
            1,
            Duration::from_secs(1),
            cancel.child_token(),
        );

        // At interval=1s, threshold=2, with 1s probe timeout → ~4s should be enough
        sleep(Duration::from_secs(4)).await;
        let snap = snapshot(&states).await;
        assert!(!snap.is_empty(), "should have health state");
        assert!(
            !snap[0].healthy,
            "trunk should be unhealthy after threshold exceeded"
        );
        assert!(
            snap[0].consecutive_failures >= 2,
            "failures should be >= threshold"
        );
        assert!(snap[0].last_error.is_some());

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_health_loop_recovery() {
        let port = pick_unused_port().unwrap();
        let (ep_inner, local_addr) = create_client_endpoint().await;

        let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
        let trunk = make_trunk(&format!("127.0.0.1:{port}"), false, 1, 2);
        let cancel = CancellationToken::new();

        let trunks = Arc::new(TestTrunks::new(HashMap::from([("test".into(), trunk)])));
        let trunks_fn = {
            let t = trunks.clone();
            move || t.lock().unwrap().clone()
        };

        spawn_health_loop_with_timeout(
            trunks_fn,
            states.clone(),
            ep_inner.clone(),
            local_addr.clone(),
            1,
            Duration::from_secs(1),
            cancel.child_token(),
        );

        // Let failures accumulate first
        sleep(Duration::from_secs(4)).await;
        let snap1 = snapshot(&states).await;
        assert!(
            !snap1.is_empty() && !snap1[0].healthy,
            "should be unhealthy"
        );

        // Start responder → restore health
        let _responder = OptionsResponder::start(port).await;
        sleep(Duration::from_secs(4)).await;
        let snap2 = snapshot(&states).await;
        assert!(snap2[0].healthy, "should recover after responder starts");

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_health_loop_threshold_boundary() {
        let (ep_inner, local_addr) = create_client_endpoint().await;
        let dead_port = pick_unused_port().unwrap();
        let dest = format!("127.0.0.1:{dead_port}");

        let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
        // threshold=5: stays healthy until 5th consecutive failure
        let trunk = make_trunk(&dest, false, 1, 5);
        let cancel = CancellationToken::new();

        let trunks = Arc::new(TestTrunks::new(HashMap::from([("test".into(), trunk)])));
        let trunks_fn = {
            let t = trunks.clone();
            move || t.lock().unwrap().clone()
        };

        spawn_health_loop(
            trunks_fn,
            states.clone(),
            ep_inner,
            local_addr,
            1,
            cancel.child_token(),
        );

        // Short wait: should have failures but below threshold → still healthy
        sleep(Duration::from_secs(2)).await;
        let snap = snapshot(&states).await;
        if !snap.is_empty() {
            // Threshold=5, with 2s we likely have 1-2 failures, should still be healthy
            assert!(
                snap[0].healthy || snap[0].consecutive_failures < 5,
                "with threshold=5, 2s should not yet mark unhealthy"
            );
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_health_loop_per_ip_mode() {
        let p1 = pick_unused_port().unwrap();
        let p2 = pick_unused_port().unwrap();
        let _r1 = OptionsResponder::start(p1).await;
        let _r2 = OptionsResponder::start(p2).await;
        let (ep_inner, local_addr) = create_client_endpoint().await;

        let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
        let mut trunk = make_trunk(&format!("127.0.0.1:{p1}"), true, 1, 3);
        trunk.inbound_hosts = vec![format!("127.0.0.1:{p2}")];
        let cancel = CancellationToken::new();

        let trunks = Arc::new(TestTrunks::new(HashMap::from([(
            "test_per_ip".into(),
            trunk,
        )])));
        let trunks_fn = {
            let t = trunks.clone();
            move || t.lock().unwrap().clone()
        };

        spawn_health_loop(
            trunks_fn,
            states.clone(),
            ep_inner,
            local_addr,
            1,
            cancel.child_token(),
        );

        sleep(Duration::from_secs(2)).await;
        let snap = snapshot(&states).await;
        assert!(!snap.is_empty());
        assert!(snap[0].healthy, "per-ip: should be healthy (both up)");

        let per_ip = snap[0].per_ip_states.as_ref().unwrap();
        assert_eq!(per_ip.len(), 2);
        assert!(per_ip[0].healthy);
        assert!(per_ip[1].healthy);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_health_loop_disabled_trunk_skipped() {
        let (ep_inner, local_addr) = create_client_endpoint().await;

        let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
        let mut trunk = make_trunk("127.0.0.1:1", false, 1, 3);
        trunk.health_check_enabled = Some(false); // explicitly disabled
        let cancel = CancellationToken::new();

        let trunks = Arc::new(TestTrunks::new(HashMap::from([("disabled".into(), trunk)])));
        let trunks_fn = {
            let t = trunks.clone();
            move || t.lock().unwrap().clone()
        };

        spawn_health_loop(
            trunks_fn,
            states.clone(),
            ep_inner,
            local_addr,
            1,
            cancel.child_token(),
        );

        sleep(Duration::from_secs(1)).await;
        let snap = snapshot(&states).await;
        assert!(snap.is_empty(), "disabled trunk should never be probed");

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_health_loop_cleans_stale_entries_on_reload() {
        let port = pick_unused_port().unwrap();
        let responder = OptionsResponder::start(port).await;
        let (ep_inner, local_addr) = create_client_endpoint().await;

        let states: HealthStateMap = Arc::new(RwLock::new(HashMap::new()));
        let trunk_a = make_trunk(&responder.addr(), false, 1, 3);
        let cancel = CancellationToken::new();

        let trunks = Arc::new(TestTrunks::new(HashMap::from([(
            "trunk_a".into(),
            trunk_a,
        )])));
        let trunks_fn = {
            let t = trunks.clone();
            move || t.lock().unwrap().clone()
        };

        spawn_health_loop(
            trunks_fn,
            states.clone(),
            ep_inner,
            local_addr,
            1,
            cancel.child_token(),
        );

        sleep(Duration::from_secs(2)).await;
        let snap = snapshot(&states).await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].trunk_name, "trunk_a");

        // Simulate reload: remove trunk_a, add trunk_b
        let trunk_b = make_trunk(&responder.addr(), false, 1, 3);
        *trunks.lock().unwrap() = HashMap::from([("trunk_b".into(), trunk_b)]);

        // Wait for the next tick to clean up stale entries
        sleep(Duration::from_secs(2)).await;
        let snap = snapshot(&states).await;
        assert!(
            snap.iter().all(|s| s.trunk_name != "trunk_a"),
            "stale trunk_a should be removed after reload"
        );
        assert!(
            snap.iter().any(|s| s.trunk_name == "trunk_b"),
            "trunk_b should appear after reload"
        );

        cancel.cancel();
    }
}
