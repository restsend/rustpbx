use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{
    extract::{Path as AxumPath, State},
    response::Response,
};
use serde_json::{Value, json};
use std::sync::Arc;

fn sample_routes() -> Vec<Value> {
    vec![
        json!({
            "id": "sales-outbound-hk",
            "name": "Sales outbound (HK)",
            "description": "Sales team calls to Hong Kong and Macau dial out through weighted SIP trunks.",
            "priority": 10,
            "direction": "outbound",
            "disabled": false,
            "match": {
                "from_user": "^20(0[1-9]|[1-6][0-9])$",
                "to_host": "^((1\\d{9})|(\\+1\\d{10}))$",
                "to_user": "^(852|853)\\d{7,8}$",
                "request_uri_host": ".*",
            },
            "rewrite": {
                "to_user": "00{1}",
                "header.X-Account-Code": "sales-apac",
            },
            "action": {
                "select": "weight",
                "hash_key": serde_json::Value::Null,
                "trunks": [
                    {"name": "sip-hk-primary", "weight": 70},
                    {"name": "sip-hk-backup", "weight": 30},
                ],
            },
            "source_trunk": "internal-cluster",
            "target_trunks": ["sip-hk-primary", "sip-hk-backup"],
            "notes": [
                "Prepends international prefix before handing off to carrier",
                "Uses weighted selection with automatic failover",
            ],
            "last_modified": "2025-09-14T10:32:00Z",
            "owner": "Voice Ops",
        }),
        json!({
            "id": "support-inbound",
            "name": "Support hotline inbound",
            "description": "Inbound calls from PSTN support hotline routed to internal IVR cluster.",
            "priority": 5,
            "direction": "inbound",
            "disabled": false,
            "match": {
                "from_host": "^203\\.0\\.113\\.\\d+$",
                "to_user": "^(400|800)\\d{6}$",
            },
            "rewrite": {
                "to_user": "ivr-{1}",
                "caller": "{1}@support.pbx",
            },
            "action": {
                "select": "hash",
                "hash_key": Some("caller".to_string()),
                "trunks": [
                    {"name": "internal-cluster", "weight": 100},
                ],
            },
            "source_trunk": "pstn-local",
            "target_trunks": ["internal-cluster"],
            "notes": [
                "Sticky routing by caller to keep customers on same IVR agent pool",
                "Rewrite maps 400/800 numbers to IVR service IDs",
            ],
            "last_modified": "2025-07-22T08:05:00Z",
            "owner": "Support Ops",
        }),
        json!({
            "id": "voicebot-outbound",
            "name": "Voice bot outbound",
            "description": "AI voice bot campaign traffic with throttled CPS and backup PSTN.",
            "priority": 30,
            "direction": "outbound",
            "disabled": true,
            "match": {
                "from_user": "^30(0[1-9]|[1-9][0-9])$",
                "to_host": "^((1\\d{9})|(\\+1\\d{10}))$",
            },
            "rewrite": {
                "to_user": "1{1}",
            },
            "action": {
                "select": "rr",
                "hash_key": serde_json::Value::Null,
                "trunks": [
                    {"name": "pstn-local", "weight": 80},
                    {"name": "pstn-backup", "weight": 20},
                ],
            },
            "source_trunk": "voicebot-gateway",
            "target_trunks": ["pstn-local", "pstn-backup"],
            "notes": [
                "Paused after campaign closed",
                "Enable only during verified campaigns",
            ],
            "last_modified": "2025-08-02T12:00:00Z",
            "owner": "Growth Team",
        }),
    ]
}

pub(crate) fn sample_trunks() -> Vec<Value> {
    vec![
        json!({
            "name": "sip-hk-primary",
            "display_name": "HK Primary SIP",
            "carrier": "Pacific Voice HK",
            "dest": "sip:hk-primary.provider.com",
            "transport": "tls",
            "weight": 70,
            "status": "Healthy",
            "backup": Some("sip-hk-backup"),
            "tags": ["international", "premium"],
            "direction": ["outbound"],
            "ip_whitelist": [
                {"label": "HK POP", "cidr": "203.0.113.10/32"},
                {"label": "HK DR", "cidr": "203.0.113.12/32"},
                {"label": "CN Access", "cidr": "198.51.100.44/31"}
            ],
            "did_numbers": [
                {"number": "+85258001234", "label": "Sales hotline"},
                {"number": "+85328781234", "label": "Macau overflow"}
            ],
            "max_calls": 160,
            "max_cps": 45,
            "limits": {
                "max_concurrent": 160,
                "max_cps": 45,
                "max_call_duration": 3600,
            },
            "metrics": {
                "availability": "99.99%",
                "latency": "46 ms",
                "loss": "0.1%",
            },
            "billing": {
                "template_id": "tpl-premium-hk",
                "template_name": "Premium HK/MO outbound",
                "currency": "HKD",
                "usage": {
                    "current": {
                        "period": "2025-09",
                        "minutes": 12450.0,
                        "calls": 3620,
                        "cost": 18240.50,
                        "included_minutes": 15000.0,
                    },
                    "last_30_days": {
                        "minutes": 29870.0,
                        "calls": 8710,
                        "cost": 43650.80,
                    }
                }
            },
            "default_route": {
                "id": "sales-outbound-hk",
                "name": "Sales outbound (HK)",
                "priority": 10,
                "strategy": "Weighted",
                "fallbacks": [
                    {"name": "sip-hk-backup", "weight": 30},
                ]
            },
            "analytics": {
                "call_distribution": [
                    {"label": "Business hours", "value": 74},
                    {"label": "After hours", "value": 26}
                ],
                "recent_events": [
                    {"timestamp": "2025-09-17T02:13:00Z", "type": "alarm", "message": "Latency spike resolved automatically"},
                    {"timestamp": "2025-09-12T07:45:00Z", "type": "change", "message": "Raised CPS limit to 45"}
                ],
                "quality_history": [
                    {"period": "2025-09-18", "latency": 46, "availability": 99.99, "loss": 0.1},
                    {"period": "2025-09-17", "latency": 49, "availability": 99.95, "loss": 0.2}
                ]
            }
        }),
        json!({
            "name": "sip-hk-backup",
            "display_name": "HK Backup SIP",
            "carrier": "Pacific Voice HK",
            "dest": "sip:hk-backup.provider.com",
            "transport": "tls",
            "weight": 30,
            "status": "Warning",
            "backup": serde_json::Value::Null,
            "tags": ["international", "backup"],
            "direction": ["outbound"],
            "ip_whitelist": [
                {"label": "HK POP", "cidr": "203.0.113.20/32"},
                {"label": "SG DR", "cidr": "203.0.113.98/32"}
            ],
            "did_numbers": [],
            "max_calls": 120,
            "max_cps": 20,
            "limits": {
                "max_concurrent": 120,
                "max_cps": 20,
                "max_call_duration": 1800,
            },
            "metrics": {
                "availability": "99.90%",
                "latency": "68 ms",
                "loss": "0.7%",
            },
            "billing": {
                "template_id": "tpl-standard-hk",
                "template_name": "Standard HK failover",
                "currency": "HKD",
                "usage": {
                    "current": {
                        "period": "2025-09",
                        "minutes": 1240.0,
                        "calls": 310,
                        "cost": 1620.30,
                        "included_minutes": 2000.0,
                    },
                    "last_30_days": {
                        "minutes": 1840.0,
                        "calls": 470,
                        "cost": 2090.12,
                    }
                }
            },
            "default_route": {
                "id": "sales-outbound-hk",
                "name": "Sales outbound (HK)",
                "priority": 10,
                "strategy": "Weighted",
                "fallbacks": [
                    {"name": "sip-hk-primary", "weight": 70}
                ]
            },
            "analytics": {
                "call_distribution": [
                    {"label": "Failover", "value": 100}
                ],
                "recent_events": [
                    {"timestamp": "2025-09-16T11:02:00Z", "type": "warning", "message": "Packet loss above 0.5%"},
                    {"timestamp": "2025-09-15T08:00:00Z", "type": "maintenance", "message": "Carrier maintenance window"}
                ],
                "quality_history": [
                    {"period": "2025-09-18", "latency": 68, "availability": 99.90, "loss": 0.7},
                    {"period": "2025-09-17", "latency": 70, "availability": 99.88, "loss": 1.1}
                ]
            }
        }),
        json!({
            "name": "pstn-local",
            "display_name": "Local PSTN",
            "carrier": "MetroTel",
            "dest": "sip:pstn-local.provider.com",
            "transport": "udp",
            "weight": 50,
            "status": "Healthy",
            "backup": Some("pstn-backup"),
            "tags": ["domestic"],
            "direction": ["inbound", "outbound"],
            "ip_whitelist": [
                {"label": "Metro POP", "cidr": "198.51.100.20/29"},
                {"label": "Firewall VIP", "cidr": "198.51.100.48/32"}
            ],
            "did_numbers": [
                {"number": "+861059001234", "label": "Support hotline"},
                {"number": "+861059001235", "label": "Billing hotline"},
                {"number": "+861059001236", "label": "VIP queue"}
            ],
            "max_calls": 90,
            "max_cps": 15,
            "limits": {
                "max_concurrent": 90,
                "max_cps": 15,
                "max_call_duration": 5400,
            },
            "metrics": {
                "availability": "99.95%",
                "latency": "32 ms",
                "loss": "0.2%",
            },
            "billing": {
                "template_id": "tpl-domestic-cn",
                "template_name": "Domestic CN metro",
                "currency": "CNY",
                "usage": {
                    "current": {
                        "period": "2025-09",
                        "minutes": 8420.0,
                        "calls": 5400,
                        "cost": 6230.75,
                        "included_minutes": 10000.0,
                    },
                    "last_30_days": {
                        "minutes": 16500.0,
                        "calls": 10800,
                        "cost": 12640.10,
                    }
                }
            },
            "default_route": {
                "id": "support-inbound",
                "name": "Support hotline inbound",
                "priority": 5,
                "strategy": "Hash",
                "fallbacks": [
                    {"name": "internal-cluster", "weight": 100}
                ]
            },
            "analytics": {
                "call_distribution": [
                    {"label": "Inbound", "value": 68},
                    {"label": "Outbound", "value": 32}
                ],
                "recent_events": [
                    {"timestamp": "2025-09-18T03:54:00Z", "type": "change", "message": "Added new DID for VIP"},
                    {"timestamp": "2025-09-10T09:10:00Z", "type": "note", "message": "Carrier confirmed new pricing"}
                ],
                "quality_history": [
                    {"period": "2025-09-18", "latency": 32, "availability": 99.95, "loss": 0.2},
                    {"period": "2025-09-17", "latency": 30, "availability": 99.97, "loss": 0.1}
                ]
            }
        }),
        json!({
            "name": "pstn-backup",
            "display_name": "Backup PSTN",
            "carrier": "MetroTel",
            "dest": "sip:pstn-backup.provider.net",
            "transport": "udp",
            "weight": 10,
            "status": "Standby",
            "backup": serde_json::Value::Null,
            "tags": ["backup", "low-cost"],
            "direction": ["inbound"],
            "ip_whitelist": [
                {"label": "Metro DR", "cidr": "198.51.100.60/30"}
            ],
            "did_numbers": [
                {"number": "+861059001240", "label": "Support failover"}
            ],
            "max_calls": 40,
            "max_cps": 8,
            "limits": {
                "max_concurrent": 40,
                "max_cps": 8,
                "max_call_duration": 3600,
            },
            "metrics": {
                "availability": "99.10%",
                "latency": "58 ms",
                "loss": "0.9%",
            },
            "billing": {
                "template_id": "tpl-backup-cn",
                "template_name": "Backup inbound CN",
                "currency": "CNY",
                "usage": {
                    "current": {
                        "period": "2025-09",
                        "minutes": 220.0,
                        "calls": 72,
                        "cost": 180.40,
                        "included_minutes": 1000.0,
                    },
                    "last_30_days": {
                        "minutes": 540.0,
                        "calls": 160,
                        "cost": 420.50,
                    }
                }
            },
            "default_route": {
                "id": "support-inbound",
                "name": "Support hotline inbound",
                "priority": 5,
                "strategy": "Hash",
                "fallbacks": [
                    {"name": "internal-cluster", "weight": 100}
                ]
            },
            "analytics": {
                "call_distribution": [
                    {"label": "Failover", "value": 100}
                ],
                "recent_events": [
                    {"timestamp": "2025-09-14T06:22:00Z", "type": "drill", "message": "Failover test completed"}
                ],
                "quality_history": [
                    {"period": "2025-09-18", "latency": 58, "availability": 99.10, "loss": 0.9},
                    {"period": "2025-09-17", "latency": 57, "availability": 99.30, "loss": 0.8}
                ]
            }
        }),
    ]
}

fn blank_route() -> Value {
    json!({
        "id": serde_json::Value::Null,
        "name": "",
        "description": "",
        "priority": 10,
        "direction": "outbound",
        "disabled": false,
        "match": {
            "from_user": "",
            "from_host": "",
            "to_user": "",
            "to_host": "",
            "request_uri_user": "",
            "request_uri_host": "",
            "request_uri_port": "",
        },
        "rewrite": {
            "from_user": "",
            "from_host": "",
            "to_user": "",
            "to_host": "",
            "request_uri_user": "",
            "request_uri_host": "",
        },
        "action": {
            "select": "rr",
            "hash_key": serde_json::Value::Null,
            "trunks": serde_json::Value::Array(vec![]),
        },
        "source_trunk": serde_json::Value::Null,
        "target_trunks": serde_json::Value::Array(vec![]),
        "notes": serde_json::Value::Array(vec![]),
        "owner": "",
    })
}

fn selection_algorithms() -> Vec<Value> {
    vec![
        json!({"value": "rr", "label": "Round robin"}),
        json!({"value": "weight", "label": "Weighted"}),
        json!({"value": "hash", "label": "Deterministic hash"}),
    ]
}

pub async fn page_routing(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let mut routes = sample_routes();
    let summary = json!({
        "total_routes": routes.len(),
        "active_routes": routes
            .iter()
            .filter(|r| r.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false) == false)
            .count(),
        "last_deploy": "2025-09-18T06:42:00Z",
    });

    for route in routes.iter_mut() {
        if let Some(obj) = route.as_object_mut() {
            if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                obj.insert(
                    "edit_url".to_string(),
                    json!(state.url_for(&format!("/routing/{id}"))),
                );
            }
        }
    }

    let payload = json!({
        "routes": routes,
        "summary": summary,
    });

    state.render(
        "console/routing.html",
        json!({
            "nav_active": "routing",
            "routing_data": payload,
            "create_url": state.url_for("/routing/new"),
        }),
    )
}

pub async fn page_routing_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let route = blank_route();
    let trunks = sample_trunks();

    state.render(
        "console/routing_form.html",
        json!({
            "nav_active": "routing",
            "mode": "create",
            "page_title": "Create routing rule",
            "submit_label": "Create routing rule",
            "route_data": serde_json::to_string(&route).unwrap_or_default(),
            "trunk_options": serde_json::to_string(&trunks).unwrap_or_default(),
            "selection_algorithms": selection_algorithms(),
            "direction_options": ["inbound", "outbound"],
            "status_options": [
                {"value": false, "label": "Active"},
                {"value": true, "label": "Paused"},
            ],
            "form_action": state.url_for("/routing"),
            "back_url": state.url_for("/routing"),
        }),
    )
}

pub async fn page_routing_edit(
    AxumPath(id): AxumPath<String>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let route = sample_routes()
        .into_iter()
        .find(|route| route.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        .unwrap_or_else(blank_route);

    let trunks = sample_trunks();

    state.render(
        "console/routing_form.html",
        json!({
            "nav_active": "routing",
            "mode": "edit",
            "page_title": format!("Edit routing · {}", route.get("name").and_then(|v| v.as_str()).unwrap_or("Unnamed")),
            "submit_label": "Save changes",
            "route_data": serde_json::to_string(&route).unwrap_or_default(),
            "trunk_options": serde_json::to_string(&trunks).unwrap_or_default(),
            "selection_algorithms": selection_algorithms(),
            "direction_options": ["inbound", "outbound"],
            "status_options": [
                {"value": false, "label": "Active"},
                {"value": true, "label": "Paused"},
            ],
            "form_action": state.url_for(&format!("/routing/{}", id)),
            "back_url": state.url_for("/routing"),
        }),
    )
}
