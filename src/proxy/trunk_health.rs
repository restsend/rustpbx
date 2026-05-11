/// Trunk health check — periodic SIP OPTIONS probing via rsipstack transaction.
///
/// Each enabled trunk receives OPTIONS requests at a configurable interval.
/// Consecutive failures beyond `probe_count` mark the trunk as unhealthy,
/// triggering automatic failover to `fallback_trunk`. When OPTIONS start
/// succeeding again the trunk is restored to healthy.
use crate::proxy::routing::TrunkConfig;
use rsipstack::sip::{
    Method, Param, Request, SipMessage, Uri, Version,
    headers::typed::CSeq,
    headers::CallId,
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
}

/// Shared health state map: trunk_name → TrunkHealthState
pub type HealthStateMap = Arc<RwLock<HashMap<String, TrunkHealthState>>>;

/// Probe a single trunk via rsipstack transaction.
/// Returns `Ok(rtt_ms)` on success, `Err(message)` on failure.
async fn probe_trunk(
    dest: &str,
    endpoint_inner: &EndpointInnerRef,
    local_sip_addr: &str,
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
    let call_id = CallId::new(format!("hc-{}", uuid::Uuid::new_v4().to_string().replace('-', "")));

    let via = Via::parse(&format!(
        "SIP/2.0/UDP {};branch={}",
        local_sip_addr, branch
    ))
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
            CSeq { seq: 1, method: Method::Options }.into(),
        ]
        .into(),
        body: vec![],
    };

    let start = std::time::Instant::now();
    let key = TransactionKey::from_request(&request, TransactionRole::Client)
        .map_err(|e| format!("transaction key: {}", e))?;
    let mut tx = Transaction::new_client(key, request, endpoint_inner.clone(), None);
    tx.send().await.map_err(|e| format!("tx send: {}", e))?;

    let timeout_dur = Duration::from_secs(10);
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

fn dest_name(dest: &str) -> &str {
    let dest = dest.trim();
    dest.strip_prefix("sip:").unwrap_or(dest)
}

/// Spawn the background health-check loop.
///
/// It reads `trunks_fn` (which returns the current trunk map) on each tick,
/// probes every trunk that has `health_check_enabled`, and updates the shared
/// `states` map.
pub fn spawn_health_loop(
    trunks_fn: impl Fn() -> HashMap<String, TrunkConfig> + Send + 'static,
    states: HealthStateMap,
    endpoint_inner: EndpointInnerRef,
    local_sip_addr: String,
    default_interval_secs: u64,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
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

                let interval = cfg.health_check_interval_secs.unwrap_or(default_interval_secs);
                next_tick = next_tick.min(interval);

                // Use trunk dest for the probe URI
                let trunk_dest = dest_name(&cfg.dest);

                match probe_trunk(trunk_dest, &endpoint_inner, &local_sip_addr).await {
                    Ok(rtt) => {
                        let mut map = states.write().await;
                        map.insert(name.clone(), TrunkHealthState {
                            trunk_name: name.clone(),
                            healthy: true,
                            consecutive_failures: 0,
                            rtt_ms: Some(rtt),
                            last_checked: Some(chrono::Utc::now().to_rfc3339()),
                            last_error: None,
                            dest: cfg.dest.clone(),
                            fallback_trunk: cfg.health_check_fallback_trunk.clone(),
                        });
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

                        map.insert(name.clone(), TrunkHealthState {
                            trunk_name: name.clone(),
                            healthy: !is_unhealthy,
                            consecutive_failures: failures,
                            rtt_ms: None,
                            last_checked: Some(chrono::Utc::now().to_rfc3339()),
                            last_error: Some(format!("OPTIONS failed: {}", e)),
                            dest: cfg.dest.clone(),
                            fallback_trunk: cfg.health_check_fallback_trunk.clone(),
                        });
                        drop(map);
                        debug!("health FAIL {}  {}  ({}/{})", name, e, failures, threshold);
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(next_tick)).await;
        }

        info!("trunk health check stopped");
    });
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
            map.insert("b".into(), TrunkHealthState {
                trunk_name: "b".into(), healthy: true, consecutive_failures: 0,
                rtt_ms: Some(5), last_checked: None, last_error: None,
                dest: "b.com".into(), fallback_trunk: None,
            });
            map.insert("a".into(), TrunkHealthState {
                trunk_name: "a".into(), healthy: true, consecutive_failures: 0,
                rtt_ms: Some(3), last_checked: None, last_error: None,
                dest: "a.com".into(), fallback_trunk: None,
            });
        }
        let snap = snapshot(&states).await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].trunk_name, "a");
        assert_eq!(snap[1].trunk_name, "b");
    }
}
