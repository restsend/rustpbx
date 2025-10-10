use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{extract::State, response::Response};
use serde_json::json;
use std::sync::Arc;

pub async fn page_diagnostics(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/diagnostics.html",
        json!({
            "nav_active": "diagnostics",
            "test_data": {
                "last_audit": "2025-10-10T02:40:00Z",
                "trunks": [
                    {
                        "id": "macrovox-primary",
                        "label": "Macrovox Primary",
                        "status": "healthy",
                        "latency_ms": 32,
                        "packet_loss_percent": 0.2,
                        "concurrency": { "current": 42, "limit": 80 },
                        "last_test_at": "2025-10-09T23:55:00Z",
                        "ingress_ips": ["203.0.113.14", "203.0.113.15"],
                        "egress": "sip:macrovox.sip.rustpbx.net",
                        "direction": ["outbound", "inbound"],
                        "notes": "Primary carrier with elastic CPS"
                    },
                    {
                        "id": "bluewave-backup",
                        "label": "BlueWave Backup",
                        "status": "warning",
                        "latency_ms": 58,
                        "packet_loss_percent": 1.4,
                        "concurrency": { "current": 12, "limit": 60 },
                        "last_test_at": "2025-10-10T00:12:00Z",
                        "ingress_ips": ["198.51.100.22"],
                        "egress": "sip:backup.bluewave.net",
                        "direction": ["outbound"],
                        "notes": "Burstable trunk used during peak hours"
                    },
                    {
                        "id": "internal-webrtc",
                        "label": "Internal WebRTC",
                        "status": "lab",
                        "latency_ms": 12,
                        "packet_loss_percent": 0.0,
                        "concurrency": { "current": 18, "limit": 120 },
                        "last_test_at": "2025-10-08T17:34:00Z",
                        "ingress_ips": ["10.10.10.0/24"],
                        "egress": "webrtc:internal.cluster.local",
                        "direction": ["internal"],
                        "notes": "Lab trunk for browser endpoints"
                    }
                ],
                "routing_checks": [
                    {
                        "id": "check-001",
                        "input": "+14155550123",
                        "direction": "outbound",
                        "matched_route": "US-Long-Distance",
                        "selected_trunk": "macrovox-primary",
                        "rewrites": ["strip_prefix:+1"],
                        "result": "ok",
                        "latency_ms": 35
                    },
                    {
                        "id": "check-002",
                        "input": "4008",
                        "direction": "internal",
                        "matched_route": "Support-IVR",
                        "selected_trunk": "internal-webrtc",
                        "rewrites": ["append_context:support"],
                        "result": "ok",
                        "latency_ms": 8
                    },
                    {
                        "id": "check-003",
                        "input": "+442079460000",
                        "direction": "outbound",
                        "matched_route": "EMEA-Offnet",
                        "selected_trunk": "bluewave-backup",
                        "rewrites": ["prefix:0044"],
                        "result": "warning",
                        "latency_ms": 72
                    }
                ],
                "recent_tests": [
                    {
                        "timestamp": "2025-10-10T02:10:00Z",
                        "type": "trunk",
                        "subject": "macrovox-primary",
                        "status": "pass",
                        "details": "OPTIONS ping ok · 34 ms round-trip"
                    },
                    {
                        "timestamp": "2025-10-09T23:50:00Z",
                        "type": "routing",
                        "subject": "+61370101234",
                        "status": "warning",
                        "details": "Fallback to BlueWave due to Macrovox concurrency cap"
                    },
                    {
                        "timestamp": "2025-10-09T21:15:00Z",
                        "type": "call",
                        "subject": "ext-201 → +14155559876",
                        "status": "pass",
                        "details": "31 s media loopback · MOS 4.3"
                    }
                ],
                "dialer": {
                    "default_source": "ext-101",
                    "source_options": ["ext-101", "ext-201", "qa-bot"],
                    "destination_samples": [
                        { "label": "San Francisco DID", "value": "+14155551212" },
                        { "label": "Support IVR", "value": "4008" },
                        { "label": "Echo test", "value": "*43" }
                    ],
                    "trunk_options": [
                        { "id": "macrovox-primary", "label": "Macrovox Primary" },
                        { "id": "bluewave-backup", "label": "BlueWave Backup" },
                        { "id": "internal-webrtc", "label": "Internal WebRTC" }
                    ]
                }
            }
        }),
    )
}
