use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{
    extract::{Path as AxumPath, State},
    response::Response,
};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};

fn sample_trunks() -> Vec<Value> {
    vec![
        json!({
            "name": "sip-hk-primary",
            "display_name": "HK Primary SIP",
            "carrier": "Pacific Voice HK",
            "dest": "sip:hk-primary.provider.com",
            "transport": "tls",
            "status": "Healthy",
            "direction": ["outbound"],
            "ip_whitelist": [
                {"label": "HK POP", "cidr": "203.0.113.10/32"},
                {"label": "HK DR", "cidr": "203.0.113.12/32"}
            ],
            "did_numbers": [
                {"number": "+85258001234", "label": "Sales hotline"}
            ],
            "max_cps": 45,
            "max_calls": 160,
            "limits": {
                "max_concurrent": 160,
                "max_cps": 45,
                "max_call_duration": 3600
            },
            "billing": {
                "template_id": "tpl-premium-hk",
                "usage": {
                    "current": {
                        "minutes": 12450.0,
                        "included_minutes": 15000.0
                    }
                }
            },
            "default_route": {
                "id": "sales-outbound-hk",
                "name": "Sales outbound (HK)"
            }
        }),
        json!({
            "name": "pstn-local",
            "display_name": "Local PSTN",
            "carrier": "MetroTel",
            "dest": "sip:pstn-local.provider.com",
            "transport": "udp",
            "status": "Warning",
            "direction": ["inbound", "outbound"],
            "ip_whitelist": [
                {"label": "Metro POP", "cidr": "198.51.100.20/29"},
                {"label": "Firewall VIP", "cidr": "198.51.100.48/32"}
            ],
            "did_numbers": [
                {"number": "+861059001234", "label": "Support hotline"},
                {"number": "+861059001235", "label": "Billing hotline"}
            ],
            "limits": {
                "max_concurrent": 90,
                "max_cps": 15,
                "max_call_duration": 5400
            },
            "billing": {
                "template_id": "tpl-domestic-cn",
                "usage": {
                    "current": {
                        "minutes": 8420.0,
                        "included_minutes": 10000.0
                    }
                }
            },
            "default_route": {
                "id": "support-inbound",
                "name": "Support inbound"
            }
        }),
    ]
}

fn direction_flags(value: &Value) -> (bool, bool) {
    let Some(arr) = value.as_array() else {
        return (false, false);
    };
    let mut inbound = false;
    let mut outbound = false;
    for item in arr {
        if let Some(dir) = item.as_str() {
            match dir {
                "inbound" => inbound = true,
                "outbound" => outbound = true,
                "bidirectional" => {
                    inbound = true;
                    outbound = true;
                }
                _ => {}
            }
        }
    }
    (inbound, outbound)
}

fn safe_count(value: Option<&Value>) -> usize {
    value
        .and_then(|v| v.as_array())
        .map(|arr| arr.len())
        .unwrap_or(0)
}

fn utilisation_ratio(trunk: &Value) -> Option<f64> {
    trunk
        .get("billing")
        .and_then(|billing| billing.get("usage"))
        .and_then(|usage| usage.get("current"))
        .and_then(|current| {
            let minutes = current.get("minutes").and_then(|v| v.as_f64())?;
            let included = current
                .get("included_minutes")
                .and_then(|v| v.as_f64())
                .filter(|v| *v > 0.0)?;
            Some((minutes / included).clamp(0.0, 1.0))
        })
}

pub async fn page_sip_trunk(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let mut trunks = sample_trunks();
    let mut inbound_count = 0usize;
    let mut outbound_count = 0usize;
    let mut bidirectional_count = 0usize;
    let mut total_cps = 0f64;
    let mut total_concurrency = 0f64;
    let mut total_ips = 0usize;
    let mut total_dids = 0usize;
    let mut warning_count = 0usize;
    let mut unique_templates = HashSet::new();
    let mut utilisation_total = 0f64;
    let mut utilisation_samples = 0usize;

    for trunk in &trunks {
        if let Some(obj) = trunk.as_object() {
            let (inbound, outbound) = obj
                .get("direction")
                .map(direction_flags)
                .unwrap_or((false, false));

            if inbound {
                inbound_count += 1;
            }
            if outbound {
                outbound_count += 1;
            }
            if inbound && outbound {
                bidirectional_count += 1;
            }

            total_ips += safe_count(obj.get("ip_whitelist"));
            total_dids += safe_count(obj.get("did_numbers"));

            if let Some(status) = obj
                .get("status")
                .and_then(|status| status.as_str())
                .map(|s| s.eq_ignore_ascii_case("warning"))
            {
                if status {
                    warning_count += 1;
                }
            }

            if let Some(template_id) = obj
                .get("billing")
                .and_then(|billing| billing.get("template_id"))
                .and_then(|id| id.as_str())
            {
                unique_templates.insert(template_id.to_string());
            }

            if let Some(max_cps) = obj.get("max_cps").and_then(|v| v.as_f64()).or_else(|| {
                obj.get("limits")
                    .and_then(|limits| limits.get("max_cps"))
                    .and_then(|v| v.as_f64())
            }) {
                total_cps += max_cps;
            }

            if let Some(max_conc) = obj.get("max_calls").and_then(|v| v.as_f64()).or_else(|| {
                obj.get("limits")
                    .and_then(|limits| limits.get("max_concurrent"))
                    .and_then(|v| v.as_f64())
            }) {
                total_concurrency += max_conc;
            }

            if let Some(ratio) = utilisation_ratio(trunk) {
                utilisation_total += ratio;
                utilisation_samples += 1;
            }
        }
    }

    let total = trunks.len();
    let avg_cps = if total > 0 {
        total_cps / total as f64
    } else {
        0.0
    };
    let avg_concurrency = if total > 0 {
        total_concurrency / total as f64
    } else {
        0.0
    };
    let avg_utilisation = if utilisation_samples > 0 {
        utilisation_total / utilisation_samples as f64
    } else {
        0.0
    };

    for trunk in &mut trunks {
        if let Some(obj) = trunk.as_object_mut() {
            if let Some(name) = obj
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|name| !name.is_empty())
            {
                obj.insert(
                    "detail_url".to_string(),
                    json!(state.url_for(&format!("/sip-trunk/{name}"))),
                );
            }

            if let Some(default_route) = obj
                .get_mut("default_route")
                .and_then(|value| value.as_object_mut())
            {
                if let Some(route_id) = default_route
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|id| !id.is_empty())
                {
                    default_route.insert(
                        "url".to_string(),
                        json!(state.url_for(&format!("/routing/{route_id}"))),
                    );
                }
            }
        }
    }

    let summary = json!({
        "total": total,
        "inbound": inbound_count,
        "outbound": outbound_count,
        "bidirectional": bidirectional_count,
        "warning": warning_count,
        "total_allowed_ips": total_ips,
        "total_dids": total_dids,
        "avg_cps": (avg_cps * 10.0).round() / 10.0,
        "avg_concurrency": avg_concurrency.round(),
        "avg_utilisation": (avg_utilisation * 1000.0).round() / 10.0,
        "unique_templates": unique_templates.len(),
    });

    let payload = json!({
        "trunks": trunks,
        "summary": summary,
    });

    state.render(
        "console/sip_trunk.html",
        json!({
            "nav_active": "sip-trunk",
            "page_title": "SIP trunk catalogue",
            "trunk_data": serde_json::to_string(&payload).unwrap_or_default(),
            "create_url": state.url_for("/routing/new"),
        }),
    )
}

pub async fn page_sip_trunk_detail(
    AxumPath(id): AxumPath<String>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let fallback_trunk = {
        let id_ref = id.clone();
        json!({
            "name": id_ref,
            "display_name": "Unknown trunk",
            "status": "Unknown",
            "direction": ["inbound"],
            "ip_whitelist": [],
            "did_numbers": [],
            "limits": {
                "max_concurrent": 0,
                "max_cps": 0,
                "max_call_duration": 0,
            },
            "billing": {
                "template_name": "N/A",
                "template_id": Value::Null,
                "usage": {
                    "current": {
                        "period": "",
                        "minutes": 0,
                        "cost": 0.0,
                        "calls": 0,
                        "included_minutes": 0,
                    }
                }
            },
            "default_route": {
                "id": Value::Null,
                "name": "",
                "priority": Value::Null,
                "url": Value::Null,
                "fallbacks": [],
            },
            "analytics": {
                "call_distribution": [],
                "recent_events": [],
                "quality_history": [],
            }
        })
    };

    let mut trunk = sample_trunks()
        .into_iter()
        .find(|trunk| {
            trunk
                .get("name")
                .and_then(|v| v.as_str())
                .map(|name| name.eq_ignore_ascii_case(&id))
                .unwrap_or(false)
        })
        .unwrap_or(fallback_trunk);

    if let Some(obj) = trunk.as_object_mut() {
        obj.insert(
            "detail_url".to_string(),
            json!(state.url_for(&format!("/sip-trunk/{id}"))),
        );
        obj.insert("back_url".to_string(), json!(state.url_for("/sip-trunk")));

        if let Some(default_route) = obj
            .get_mut("default_route")
            .and_then(|value| value.as_object_mut())
        {
            if let Some(route_id) = default_route
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                default_route.insert(
                    "url".to_string(),
                    json!(state.url_for(&format!("/routing/{route_id}"))),
                );
            }
        }
    }

    let utilisation_percent = utilisation_ratio(&trunk)
        .map(|ratio| (ratio * 1000.0).round() / 10.0)
        .unwrap_or(0.0);

    let cost = trunk
        .get("billing")
        .and_then(|billing| billing.get("usage"))
        .and_then(|usage| usage.get("current"))
        .and_then(|current| current.get("cost"))
        .and_then(|cost| cost.as_f64())
        .unwrap_or(0.0);
    let minutes = trunk
        .get("billing")
        .and_then(|billing| billing.get("usage"))
        .and_then(|usage| usage.get("current"))
        .and_then(|current| current.get("minutes"))
        .and_then(|minutes| minutes.as_f64())
        .unwrap_or(0.0);

    let summary = json!({
        "utilisation_percent": utilisation_percent,
        "current_cost": (cost * 100.0).round() / 100.0,
        "current_minutes": minutes,
    });

    let payload = json!({
        "trunk": trunk,
        "summary": summary,
        "back_url": state.url_for("/sip-trunk"),
    });

    state.render(
        "console/sip_trunk_detail.html",
        json!({
            "nav_active": "sip-trunk-detail",
            "page_title": format!("SIP trunk · {}", id),
            "trunk_name": id,
            "trunk_data": serde_json::to_string(&payload).unwrap_or_default(),
        }),
    )
}
