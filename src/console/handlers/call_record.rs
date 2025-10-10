use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{
    extract::{Path as AxumPath, State},
    response::Response,
};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};

const SAMPLE_AUDIO_URI: &str =
    "data:audio/wav;base64,UklGRiQAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA=";

fn sample_call_records() -> Vec<Value> {
    vec![
        json!({
            "id": "cr-2025-09-18-001",
            "display_id": "CR-0925-001",
            "direction": "inbound",
            "from": "+852 5800 1234",
            "to": "800 123 456",
            "started_at": "2025-09-18T08:30:25Z",
            "ended_at": "2025-09-18T08:36:21Z",
            "duration_secs": 356,
            "status": "completed",
            "queue": "Support · VIP",
            "agent": "Evelyn Zhang",
            "agent_extension": "2088",
            "sip_gateway": "sip-hk-primary",
            "codec": "OPUS/48000",
            "cnam": "Ms. Lee",
            "tags": ["support", "vip"],
            "has_transcript": true,
            "csat": 4,
            "quality": {
                "mos": 4.3,
                "packet_loss": 0.2,
                "jitter_ms": 12,
            },
            "recording": {
                "format": "wav",
                "url": SAMPLE_AUDIO_URI,
                "size_bytes": 24576,
                "duration_secs": 356,
            },
        }),
        json!({
            "id": "cr-2025-09-17-014",
            "display_id": "CR-0917-014",
            "direction": "outbound",
            "from": "2075",
            "to": "+1 415 555 0102",
            "started_at": "2025-09-17T13:12:04Z",
            "ended_at": "2025-09-17T13:19:43Z",
            "duration_secs": 459,
            "status": "completed",
            "queue": "Sales · Enterprise",
            "agent": "Marcus Chen",
            "agent_extension": "2075",
            "sip_gateway": "pstn-local",
            "codec": "G722",
            "cnam": "Contoso Labs",
            "tags": ["sales", "demo"],
            "has_transcript": false,
            "csat": 5,
            "quality": {
                "mos": 4.5,
                "packet_loss": 0.1,
                "jitter_ms": 9,
            },
            "recording": {
                "format": "wav",
                "url": SAMPLE_AUDIO_URI,
                "size_bytes": 31542,
                "duration_secs": 459,
            },
        }),
        json!({
            "id": "cr-2025-09-17-022",
            "display_id": "CR-0917-022",
            "direction": "inbound",
            "from": "+86 1088 001 888",
            "to": "400 660 8800",
            "started_at": "2025-09-17T03:42:10Z",
            "ended_at": "2025-09-17T03:42:32Z",
            "duration_secs": 22,
            "status": "missed",
            "queue": "Support · General",
            "agent": "Queue",
            "agent_extension": Value::Null,
            "sip_gateway": "pstn-backup",
            "codec": "PCMU",
            "cnam": "Unknown",
            "tags": ["missed", "after-hours"],
            "has_transcript": false,
            "csat": Value::Null,
            "quality": {
                "mos": 3.8,
                "packet_loss": 0.0,
                "jitter_ms": 4,
            },
            "recording": Value::Null,
        }),
        json!({
            "id": "cr-2025-09-16-008",
            "display_id": "CR-0916-008",
            "direction": "internal",
            "from": "2010",
            "to": "2012",
            "started_at": "2025-09-16T10:02:18Z",
            "ended_at": "2025-09-16T10:04:01Z",
            "duration_secs": 103,
            "status": "failed",
            "queue": "Ops Bridge",
            "agent": "Automation",
            "agent_extension": "BOT",
            "sip_gateway": "internal-cluster",
            "codec": "OPUS/48000",
            "cnam": "Voicebot",
            "tags": ["automation", "alert"],
            "has_transcript": true,
            "csat": Value::Null,
            "quality": {
                "mos": 3.1,
                "packet_loss": 0.6,
                "jitter_ms": 28,
            },
            "recording": {
                "format": "wav",
                "url": SAMPLE_AUDIO_URI,
                "size_bytes": 10240,
                "duration_secs": 103,
            },
        }),
    ]
}

fn sample_call_detail(id: &str) -> Value {
    let record = sample_call_records()
        .into_iter()
        .find(|value| {
            value
                .get("id")
                .and_then(|v| v.as_str())
                .map(|candidate| candidate.eq_ignore_ascii_case(id))
                .unwrap_or(false)
        })
        .unwrap_or_else(|| {
            json!({
                "id": id,
                "display_id": id,
                "direction": "inbound",
                "from": "Unknown",
                "to": "Unknown",
                "started_at": "2025-09-18T00:00:00Z",
                "ended_at": Value::Null,
                "duration_secs": 0,
                "status": "unknown",
                "queue": Value::Null,
                "agent": Value::Null,
                "agent_extension": Value::Null,
                "sip_gateway": Value::Null,
                "codec": "OPUS/48000",
                "cnam": Value::Null,
                "tags": Value::Array(vec![]),
                "has_transcript": false,
                "csat": Value::Null,
                "quality": Value::Null,
                "recording": Value::Null,
            })
        });

    let has_transcript = record
        .get("has_transcript")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let sip_flow = match id {
        "cr-2025-09-18-001" => vec![
            json!({
                "timestamp": "2025-09-18T08:30:25.120Z",
                "offset": "00:00.000",
                "direction": "inbound",
                "type": "request",
                "method": "INVITE",
                "status_code": Value::Null,
                "summary": "Carrier INVITE routed to support VIP queue (codec OPUS).",
                "raw": "INVITE sip:800123456@pbx.rustpbx.local SIP/2.0\r\nVia: SIP/2.0/TLS carrier.net;branch=z9hG4bK...",
            }),
            json!({
                "timestamp": "2025-09-18T08:30:25.248Z",
                "offset": "00:00.128",
                "direction": "internal",
                "type": "response",
                "method": "TRYING",
                "status_code": 100,
                "summary": "PBX responded 100 Trying to acknowledge INVITE.",
                "raw": "SIP/2.0 100 Trying\r\nVia: SIP/2.0/TLS carrier.net;branch=z9hG4bK...",
            }),
            json!({
                "timestamp": "2025-09-18T08:30:28.032Z",
                "offset": "00:02.912",
                "direction": "internal",
                "type": "response",
                "method": "RINGING",
                "status_code": 180,
                "summary": "Agent extension 2088 alerted.",
                "raw": "SIP/2.0 180 Ringing\r\nContact: <sip:2088@10.10.1.50>...",
            }),
            json!({
                "timestamp": "2025-09-18T08:30:31.877Z",
                "offset": "00:06.757",
                "direction": "internal",
                "type": "response",
                "method": "OK",
                "status_code": 200,
                "summary": "Agent answered, SDP answer negotiated (OPUS).",
                "raw": "SIP/2.0 200 OK\r\nContact: <sip:2088@10.10.1.50>\r\nContent-Type: application/sdp\r\n...",
            }),
            json!({
                "timestamp": "2025-09-18T08:30:31.981Z",
                "offset": "00:06.861",
                "direction": "inbound",
                "type": "request",
                "method": "ACK",
                "status_code": Value::Null,
                "summary": "Carrier ACK confirming dialog establishment.",
                "raw": "ACK sip:2088@10.10.1.50 SIP/2.0\r\nVia: SIP/2.0/TLS carrier.net...",
            }),
            json!({
                "timestamp": "2025-09-18T08:36:21.420Z",
                "offset": "06:56.300",
                "direction": "inbound",
                "type": "request",
                "method": "BYE",
                "status_code": Value::Null,
                "summary": "Caller hung up after resolution.",
                "raw": "BYE sip:800123456@pbx.rustpbx.local SIP/2.0\r\nVia: SIP/2.0/TLS carrier.net...",
            }),
            json!({
                "timestamp": "2025-09-18T08:36:21.512Z",
                "offset": "06:56.392",
                "direction": "internal",
                "type": "response",
                "method": "OK",
                "status_code": 200,
                "summary": "PBX acknowledged BYE and released channels.",
                "raw": "SIP/2.0 200 OK\r\nVia: SIP/2.0/TLS carrier.net...",
            }),
        ],
        "cr-2025-09-17-014" => vec![
            json!({
                "timestamp": "2025-09-17T13:12:04.010Z",
                "offset": "00:00.000",
                "direction": "outbound",
                "type": "request",
                "method": "INVITE",
                "status_code": Value::Null,
                "summary": "Agent 2075 dialed customer via pstn-local trunk.",
                "raw": "INVITE sip:+14155550102@pstn.local SIP/2.0\r\nFrom: <sip:2075@pbx>...",
            }),
            json!({
                "timestamp": "2025-09-17T13:12:04.120Z",
                "offset": "00:00.110",
                "direction": "internal",
                "type": "response",
                "method": "TRYING",
                "status_code": 100,
                "summary": "Carrier acknowledged outbound INVITE.",
                "raw": "SIP/2.0 100 Trying\r\nVia: SIP/2.0/UDP 10.10.2.14;branch=...",
            }),
            json!({
                "timestamp": "2025-09-17T13:12:06.820Z",
                "offset": "00:02.810",
                "direction": "inbound",
                "type": "response",
                "method": "RINGING",
                "status_code": 180,
                "summary": "Destination ringing.",
                "raw": "SIP/2.0 180 Ringing\r\nContact: <sip:+14155550102@carrier>...",
            }),
            json!({
                "timestamp": "2025-09-17T13:12:08.031Z",
                "offset": "00:04.021",
                "direction": "inbound",
                "type": "response",
                "method": "OK",
                "status_code": 200,
                "summary": "Customer answered, codec negotiated to G722.",
                "raw": "SIP/2.0 200 OK\r\nContent-Type: application/sdp\r\n...",
            }),
            json!({
                "timestamp": "2025-09-17T13:19:43.812Z",
                "offset": "07:39.802",
                "direction": "outbound",
                "type": "request",
                "method": "BYE",
                "status_code": Value::Null,
                "summary": "Agent terminated call after demo wrap-up.",
                "raw": "BYE sip:+14155550102@carrier SIP/2.0\r\nFrom: <sip:2075@pbx>...",
            }),
        ],
        "cr-2025-09-17-022" => vec![
            json!({
                "timestamp": "2025-09-17T03:42:10.004Z",
                "offset": "00:00.000",
                "direction": "inbound",
                "type": "request",
                "method": "INVITE",
                "status_code": Value::Null,
                "summary": "Inbound hotline call outside staffed hours.",
                "raw": "INVITE sip:4006608800@pbx SIP/2.0\r\n...",
            }),
            json!({
                "timestamp": "2025-09-17T03:42:10.118Z",
                "offset": "00:00.114",
                "direction": "internal",
                "type": "response",
                "method": "TRYING",
                "status_code": 100,
                "summary": "PBX acknowledged and pushed to queue timer.",
                "raw": "SIP/2.0 100 Trying\r\n...",
            }),
            json!({
                "timestamp": "2025-09-17T03:42:32.440Z",
                "offset": "00:22.436",
                "direction": "internal",
                "type": "response",
                "method": "DECLINE",
                "status_code": 603,
                "summary": "Queue timeout reached, failover message played.",
                "raw": "SIP/2.0 603 Decline\r\nReason: Q.850;cause=17;text=\"User busy\"...",
            }),
        ],
        "cr-2025-09-16-008" => vec![
            json!({
                "timestamp": "2025-09-16T10:02:18.514Z",
                "offset": "00:00.000",
                "direction": "internal",
                "type": "request",
                "method": "INVITE",
                "status_code": Value::Null,
                "summary": "Automation dialed Ops on-call bridge.",
                "raw": "INVITE sip:2012@cluster.local SIP/2.0\r\n...",
            }),
            json!({
                "timestamp": "2025-09-16T10:02:18.620Z",
                "offset": "00:00.106",
                "direction": "internal",
                "type": "response",
                "method": "TRYING",
                "status_code": 100,
                "summary": "Cluster acknowledged automation INVITE.",
                "raw": "SIP/2.0 100 Trying\r\n...",
            }),
            json!({
                "timestamp": "2025-09-16T10:02:19.882Z",
                "offset": "00:01.368",
                "direction": "internal",
                "type": "response",
                "method": "SERVICE_UNAVAILABLE",
                "status_code": 503,
                "summary": "Endpoint 2012 unreachable, failover triggered.",
                "raw": "SIP/2.0 503 Service Unavailable\r\nRetry-After: 120\r\n...",
            }),
        ],
        _ => vec![json!({
            "timestamp": "2025-09-18T00:00:00.000Z",
            "offset": "00:00.000",
            "direction": "internal",
            "type": "note",
            "method": "INFO",
            "status_code": Value::Null,
            "summary": "No detailed SIP trace stored for this record.",
            "raw": "",
        })],
    };

    let transcript = if has_transcript {
        json!({
            "available": true,
            "language": "zh-CN",
            "generated_at": "2025-09-18T08:40:31Z",
            "segments": [
                {"speaker": "客户", "start": "00:05.2", "text": "你好，我的账单有问题想咨询。"},
                {"speaker": "坐席", "start": "00:08.9", "text": "好的，我来帮您查一下最近的费用。"},
                {"speaker": "客户", "start": "02:14.3", "text": "我收到了一条欠费短信。"},
                {"speaker": "坐席", "start": "02:20.8", "text": "已经帮您核对，稍后会发送确认邮件。"}
            ],
            "text": "客户询问账单异常，坐席核实后确认系统误报，已补发确认邮件。",
        })
    } else {
        json!({
            "available": false,
            "language": Value::Null,
            "generated_at": Value::Null,
            "segments": Value::Array(vec![]),
            "text": "",
        })
    };

    let transcript_preview = if has_transcript {
        Value::Null
    } else {
        json!({
            "language": "en-US",
            "segments": [
                {"speaker": "Agent", "start": "00:03.6", "text": "Hi, thanks for taking the demo today."},
                {"speaker": "Customer", "start": "00:12.1", "text": "We're evaluating AI voice for our support team."},
                {"speaker": "Agent", "start": "01:45.0", "text": "I'll send over the pricing and integration playbook."}
            ],
            "text": "Agent walked through outbound demo call and promised to share follow-up collateral.",
        })
    };

    let media_metrics = json!({
        "audio_codec": record
            .get("codec")
            .and_then(|v| v.as_str())
            .unwrap_or("OPUS/48000"),
        "rtp_packets": match id {
            "cr-2025-09-18-001" => 5232,
            "cr-2025-09-17-014" => 6120,
            "cr-2025-09-16-008" => 1100,
            _ => 0,
        },
        "avg_jitter_ms": match id {
            "cr-2025-09-18-001" => 11.2,
            "cr-2025-09-17-014" => 8.4,
            "cr-2025-09-16-008" => 26.5,
            _ => 0.0,
        },
        "packet_loss_percent": match id {
            "cr-2025-09-18-001" => 0.24,
            "cr-2025-09-17-014" => 0.11,
            "cr-2025-09-16-008" => 0.62,
            _ => 0.0,
        },
        "mos": record
            .get("quality")
            .and_then(|v| v.get("mos"))
            .and_then(|v| v.as_f64())
            .unwrap_or(3.5),
        "rtcp_observations": [
            {"time": "+60s", "rtt_ms": 45, "fraction_lost": 0.1},
            {"time": "+180s", "rtt_ms": 39, "fraction_lost": 0.05}
        ],
    });

    let notes = match id {
        "cr-2025-09-18-001" => json!({
            "text": "客户账单误报，已提交工单 #SR-4921 并发送确认邮件。",
            "updated_at": "2025-09-18T09:10:00Z",
        }),
        "cr-2025-09-17-014" => json!({
            "text": "客户计划下周内部评审，等待报价模板。",
            "updated_at": "2025-09-17T14:05:12Z",
        }),
        _ => json!({
            "text": "",
            "updated_at": Value::Null,
        }),
    };

    let participants = json!([
        {
            "role": "caller",
            "label": "Caller",
            "name": record.get("cnam").and_then(|v| v.as_str()).unwrap_or("Unknown"),
            "number": record.get("from").and_then(|v| v.as_str()).unwrap_or("Unknown"),
            "network": record.get("sip_gateway").and_then(|v| v.as_str()).unwrap_or("PSTN"),
        },
        {
            "role": "agent",
            "label": "Agent",
            "name": record.get("agent").and_then(|v| v.as_str()).unwrap_or(""),
            "number": record
                .get("agent_extension")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            "network": "PBX",
        }
    ]);

    let actions = json!({
        "download_recording": record
            .get("recording")
            .and_then(|rec| rec.get("url"))
            .cloned()
            .unwrap_or(Value::Null),
        "download_metadata": format!("/console/api/call-records/{id}/metadata.json"),
        "download_sip_flow": format!("/console/api/call-records/{id}/sip-flow.json"),
    });

    json!({
        "record": record,
        "sip_flow": sip_flow,
        "media_metrics": media_metrics,
        "transcript": transcript,
        "transcript_preview": transcript_preview,
        "notes": notes,
        "participants": participants,
        "actions": actions,
    })
}

pub async fn page_call_records(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let mut records = sample_call_records();
    let mut answered = 0usize;
    let mut missed = 0usize;
    let mut failed = 0usize;
    let mut transcribed = 0usize;
    let mut total_duration_secs = 0f64;
    let mut unique_dids: HashSet<String> = HashSet::new();

    for record in records.iter_mut() {
        if let Some(obj) = record.as_object_mut() {
            if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                obj.insert(
                    "detail_url".to_string(),
                    json!(state.url_for(&format!("/call-records/{id}"))),
                );
            }
            if let Some(to_number) = obj.get("to").and_then(|v| v.as_str()) {
                unique_dids.insert(to_number.to_string());
            }
            if obj
                .get("has_transcript")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                transcribed += 1;
            }
        }

        let status = record
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_lowercase();
        match status.as_str() {
            "completed" => answered += 1,
            "missed" => missed += 1,
            "failed" => failed += 1,
            _ => {}
        }

        total_duration_secs += record
            .get("duration_secs")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
    }

    let total = records.len();
    let avg_duration = if total > 0 {
        total_duration_secs / total as f64
    } else {
        0.0
    };

    let summary = json!({
        "total": total,
        "answered": answered,
        "missed": missed,
        "failed": failed,
        "transcribed": transcribed,
        "avg_duration": (avg_duration * 10.0).round() / 10.0,
        "total_minutes": (total_duration_secs / 60.0).round(),
        "unique_dids": unique_dids.len(),
    });

    let payload = json!({
        "records": records,
        "summary": summary,
        "filter_options": {
            "status": ["any", "completed", "missed", "failed"],
            "direction": ["any", "inbound", "outbound", "internal"],
        },
    });

    state.render(
        "console/call_records.html",
        json!({
            "nav_active": "call-records",
            "page_title": "Call records",
            "call_data": serde_json::to_string(&payload).unwrap_or_default(),
        }),
    )
}

pub async fn page_call_record_detail(
    AxumPath(id): AxumPath<String>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let mut detail = sample_call_detail(&id);

    if let Some(obj) = detail.as_object_mut() {
        obj.insert(
            "back_url".to_string(),
            json!(state.url_for("/call-records")),
        );
        if let Some(record) = obj.get_mut("record").and_then(|v| v.as_object_mut()) {
            record
                .entry("detail_url".to_string())
                .or_insert_with(|| json!(state.url_for(&format!("/call-records/{id}"))));
        }
    }

    state.render(
        "console/call_record_detail.html",
        json!({
            "nav_active": "call-records",
            "page_title": format!("Call record · {}", id),
            "call_id": id,
            "call_data": serde_json::to_string(&detail).unwrap_or_default(),
            "js_files": vec!["https://cdn.jsdelivr.net/npm/@alpinejs/collapse@3.x.x/dist/cdn.min.js"],
        }),
    )
}
