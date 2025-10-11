use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{extract::State, response::Response};
use serde_json::json;
use std::sync::Arc;

pub async fn page_settings(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    state.render(
        "console/settings.html",
        json!({
            "nav_active": "settings",
            "settings_data": {
                "server": {
                    "cluster_name": "RustPBX Prod",
                    "external_fqdn": "voice.rustpbx.example",
                    "bind_addresses": ["0.0.0.0:5060", "0.0.0.0:5061", "[::]:5060"],
                    "media_bind": "10.0.12.0/24",
                    "storage": {
                        "mode": "hybrid",
                        "local_path": "/var/lib/rustpbx/media",
                        "s3": {
                            "enabled": true,
                            "bucket": "rustpbx-prod-recordings",
                            "region": "us-west-2",
                            "prefix": "voice/",
                            "access_key": "AKIA••••",
                            "endpoint": "https://s3.us-west-2.amazonaws.com"
                        }
                    },
                    "storage_profiles": [
                        {
                            "id": "local",
                            "label": "Local filesystem",
                            "description": "Store recordings and diagnostics on the PBX host with nightly rsync to cold storage.",
                            "config": {
                                "path": "/var/lib/rustpbx/media",
                                "filesystem": "ext4",
                                "capacity_gb": 512
                            }
                        },
                        {
                            "id": "s3",
                            "label": "Amazon S3 compatible",
                            "description": "Offload assets to an S3 bucket with automatic lifecycle rules.",
                            "config": {
                                "bucket": "rustpbx-prod-recordings",
                                "region": "us-west-2",
                                "prefix": "voice/",
                                "endpoint": "https://s3.us-west-2.amazonaws.com"
                            }
                        }
                    ],
                    "operations": [
                        {"label": "Reload routing", "id": "reload-routing"},
                        {"label": "Reload ACL", "id": "reload-acl"},
                    ],
                    "database": {
                        "provider": "PostgreSQL",
                        "dsn": "postgres://rustpbx:***@pg-cluster.internal:5432/pbx",
                        "pool_size": 32,
                        "replica": "pg-replica.internal:5432"
                    },
                    "last_reload": "2025-10-09T19:12:00Z",
                    "release_channel": "stable"
                },
                "retention": {
                    "call_records_days": 365,
                    "recordings_days": 180,
                    "cdr_export": {
                        "enabled": true,
                        "frequency": "daily",
                        "target": "s3://rustpbx-analytics/cdr/"
                    },
                    "anonymise_after_days": 30
                },
                "directory": {
                    "departments": [
                        {
                            "id": "dept-support",
                            "name": "Support",
                            "lead": "Alice Chen",
                            "extension_range": "4000-4099",
                            "members": 42,
                            "description": "24/7 response team covering critical incidents and VIP accounts.",
                            "tags": ["24/7", "Customer care"],
                            "sla": "30s answer"
                        },
                        {
                            "id": "dept-sales",
                            "name": "Sales",
                            "lead": "Miguel Torres",
                            "extension_range": "4200-4399",
                            "members": 28,
                            "description": "Outbound and inbound revenue team with regional breakouts.",
                            "tags": ["Pipeline", "Forecast"],
                            "sla": "FCR 92%"
                        },
                        {
                            "id": "dept-accounting",
                            "name": "Accounting",
                            "lead": "Priya Natarajan",
                            "extension_range": "4600-4699",
                            "members": 6,
                            "description": "Financial operations, billing reconciliation, and vendor payments.",
                            "tags": ["Finance"],
                            "sla": "Next business day"
                        }
                    ],
                    "users": [
                        {
                            "id": "user-alice",
                            "name": "Alice Chen",
                            "email": "alice.chen@rustpbx.example",
                            "role": "Administrator",
                            "department": "Support",
                            "status": "Active",
                            "last_login": "2025-10-09T21:30:00Z",
                            "mfa": true,
                            "extensions": ["4001"],
                            "permissions": ["Trunks", "Routing", "Settings"],
                            "avatar": "AC"
                        },
                        {
                            "id": "user-miguel",
                            "name": "Miguel Torres",
                            "email": "miguel.torres@rustpbx.example",
                            "role": "Operations",
                            "department": "Sales",
                            "status": "Active",
                            "last_login": "2025-10-09T18:05:00Z",
                            "mfa": true,
                            "extensions": ["4210", "4211"],
                            "permissions": ["Routes", "Diagnostics"],
                            "avatar": "MT"
                        },
                        {
                            "id": "user-bot",
                            "name": "QA Bot",
                            "email": "qa.bot@rustpbx.example",
                            "role": "Automation",
                            "department": "Support",
                            "status": "Suspended",
                            "last_login": "2025-10-08T11:22:00Z",
                            "mfa": false,
                            "extensions": ["lab-echo"],
                            "permissions": ["Diagnostics"],
                            "avatar": "QB"
                        }
                    ],
                    "pending_invites": [
                        {
                            "id": "invite-finance",
                            "email": "finance.lead@rustpbx.example",
                            "role": "Finance",
                            "sent_at": "2025-10-07T14:00:00Z"
                        }
                    ]
                },
                "security": {
                    "trusted_ips": ["203.0.113.10", "198.51.100.42", "10.0.0.0/16"],
                    "blocked_user_agents": ["friendly-scanner", "sundayddr", "sipcli"],
                    "rate_limits": {
                        "register_per_minute": 30,
                        "options_per_minute": 120,
                        "invite_per_minute": 60
                    },
                    "threat_feed": [
                        {"source": "AbuseIPDB", "last_sync": "2025-10-09T23:00:00Z", "entries": 1243},
                        {"source": "Spamhaus DROP", "last_sync": "2025-10-09T19:10:00Z", "entries": 842}
                    ],
                    "audit": {
                        "last_failed_login": "2025-10-09T17:44:00Z",
                        "policy_version": "2025.09"
                    }
                },
                "asr": {
                    "providers": [
                        {"id": "openai-whisper", "name": "OpenAI Whisper", "token_hint": "sk-live-••••"},
                        {"id": "google-speech", "name": "Google Speech-to-Text", "token_hint": "service-account.json"},
                        {"id": "qcloud-asr", "name": "Tencent Cloud ASR", "token_hint": "secretId/secretKey"}
                    ],
                    "active_provider": "openai-whisper",
                    "callback": {
                        "url": "https://voice.rustpbx.example/hooks/asr",
                        "auth_header": "Bearer ***"
                    },
                    "languages": ["en-US", "zh-CN", "ja-JP"],
                }
            }
        }),
    )
}
