//! E2E: in-band audio DTMF detection over the REAL call + app + RWI webhook
//! path (design: in-band DTMF §六 integration test).
//!
//! Contract: when the caller's SDP negotiates NO telephone-event
//! (`m=audio RTP/AVP 0 8` — PCMU/PCMA only), the leg runs the Goertzel
//! detector and a key press delivered as AUDIO (dual-tone PCM inside the
//! ordinary audio RTP stream, the way old phones / PSTN trunks sound) must
//! reach the running step-IVR app and surface on the RWI webhook as:
//!
//! 1. `ivr_step_trace` with `trigger = dtmf` + the pressed digit (the app
//!    consumed the digit — barge-in on the prompt), and
//! 2. a `dtmf` event whose `extra.source == "inband"` (the regression
//!    marker for cluster verification; RFC 2833 / SIP INFO sourced events
//!    carry `extra: None`).
//!
//! There is no SIP INFO and no telephone-event RTP anywhere in this flow —
//! the ONLY path a digit can take into the app is the in-band audio
//! detector. The harness mirrors `test_step_ivr_trace_webhook_e2e.rs`
//! (step-provider HTTP mock + webhook capture), with the webhook
//! allow-list extended by `dtmf` so the RWI event is observable.

use anyhow::{Result, anyhow};
use rustpbx::call::user::SipUser;
use rustpbx::config::{LocatorWebhookConfig, ProxyConfig};
use rustpbx::media::wav_reader::{SampleFormat, WavSpec, WavWriter};
use rustpbx::media::DtmfGenerator;
use rustpbx::proxy::routing::{MatchConditions, RouteAction, RouteRule};
use rustpbx::rwi::{RwiGateway, RwiGatewayRef, webhook::start_rwi_webhook_handler};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::rtp_utils::{RtpEndpoint, extract_media_endpoint};
use crate::common::test_ua::{TestUa, TestUaConfig, create_test_sdp};
use crate::common::webhook_capture::WebhookCapture;

const IVR_NUMBER: &str = "9301";
const CALLER: &str = "inband_caller";
const SAMPLE_RATE: u32 = 8000;
const PRESSED_DIGIT: char = '5';

/// Webhook allow-list for THIS scenario: the lifecycle set plus `dtmf` —
/// the RWI event whose `extra.source` marker this test asserts on.
const INBAND_WEBHOOK_EVENTS: &[&str] = &[
    "call_created",
    "call_answered",
    "ivr_step_trace",
    "call_hangup",
    "dtmf",
];

// ── Step-provider HTTP mock (same shape as the step-trace harness) ────────

struct StepProviderMock {
    url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn start_step_provider(script: Vec<serde_json::Value>) -> StepProviderMock {
    let idx = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(script);

    let idx_step = idx.clone();
    let script_step = script.clone();
    let requests_step = requests.clone();
    let app = axum::Router::new()
        .route(
            "/ivr/step",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let idx = idx_step.clone();
                let script = script_step.clone();
                let requests = requests_step.clone();
                async move {
                    requests.lock().unwrap().push(body);
                    let i = idx.fetch_add(1, Ordering::SeqCst);
                    let action = script
                        .get(i)
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "hangup"}));
                    axum::Json(action)
                }
            }),
        )
        .route(
            "/ivr/step/start",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({"status": "ok"}))
            }),
        )
        .route(
            "/ivr/step/end",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({"status": "ok"}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    StepProviderMock {
        url: format!("http://127.0.0.1:{port}/ivr/step"),
        requests,
    }
}

impl StepProviderMock {
    /// Wait until the provider observed a step request whose event sequence
    /// contains a `dtmf` input (the app-level proof of the digit).
    async fn wait_for_dtmf_event(&self, timeout: Duration) -> Result<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            for body in self.requests.lock().unwrap().iter() {
                if body["event"]["type"].as_str() == Some("dtmf") {
                    return Ok(body["event"].clone());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                let seen: Vec<String> = self
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|b| b["event"]["type"].as_str().unwrap_or("?").to_string())
                    .collect();
                return Err(anyhow!(
                    "step provider never observed a dtmf event in {timeout:?}; saw {seen:?}"
                ));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
}

// ── Harness ────────────────────────────────────────────────────────────────

fn write_sine_wav(path: &Path, freq_hz: f64, duration_ms: u64) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let num_samples = SAMPLE_RATE as u64 * duration_ms / 1000;
    let mut writer = WavWriter::create(path, spec)?;
    for i in 0..num_samples {
        let t = i as f64 / SAMPLE_RATE as f64;
        let sample = (8000.0 * (2.0 * std::f64::consts::PI * freq_hz * t).sin()) as i16;
        writer.write_sample(sample)?;
    }
    writer.finalize()?;
    Ok(())
}

fn step_ivr_toml(provider_url: &str) -> String {
    format!(
        r#"[ivr]
name = "inband-dtmf-ivr"
ivr_mode = "step"

[ivr.provider]
url = "{provider_url}"
max_retries = 2
retry_delay_ms = 200
timeout_secs = 5
"#
    )
}

async fn start_harness(capture: &WebhookCapture, ivr_file: &Path) -> Result<E2eTestServer> {
    let gateway: RwiGatewayRef = Arc::new(parking_lot::RwLock::new({
        let mut gw = RwiGateway::new();
        gw.set_webhook_tx(start_rwi_webhook_handler(
            LocatorWebhookConfig {
                url: capture.url.clone(),
                events: INBAND_WEBHOOK_EVENTS.iter().map(|s| s.to_string()).collect(),
                headers: None,
                timeout_ms: Some(5000),
                retries: None,
                track_queue_latency: None,
            },
            rustpbx::rwi::webhook::WEBHOOK_CHANNEL_SIZE,
        ));
        gw
    }));

    let mut proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(0),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    };
    proxy_config.ensure_user = Some(false);
    proxy_config.enable_latching = false;
    proxy_config.routes = Some(vec![RouteRule {
        name: "route_to_inband_dtmf_ivr".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some(IVR_NUMBER.to_string()),
            ..Default::default()
        },
        action: RouteAction {
            app: Some("ivr".to_string()),
            app_params: Some(serde_json::json!({
                "file": ivr_file.to_string_lossy(),
            })),
            ..Default::default()
        },
        ..Default::default()
    }]);

    let users = vec![SipUser {
        id: 1,
        username: CALLER.to_string(),
        password: Some("password".to_string()),
        enabled: true,
        realm: Some("127.0.0.1".to_string()),
        ..Default::default()
    }];

    E2eTestServer::start_with_inject(
        proxy_config,
        E2eTestServerInject {
            users,
            rwi_gateway: Some(gateway),
            ..Default::default()
        },
    )
    .await
}

fn make_caller_ua(proxy_addr: std::net::SocketAddr) -> TestUa {
    TestUa::new(TestUaConfig {
        webrtc: false,
        username: CALLER.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(29000),
        proxy_addr,
    })
}

/// Keep the caller UA's dialog state machine alive (BYEs / re-INVITEs).
fn spawn_caller_pump(ua: TestUa) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match ua.process_dialog_events().await {
                Ok(_) => {}
                Err(_) => break,
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
}

async fn wait_webhook_event(
    capture: &WebhookCapture,
    event_type: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        {
            let events = capture.received.lock().unwrap();
            if events
                .iter()
                .any(|v| v["event_type"].as_str() == Some(event_type))
            {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let types: Vec<String> = {
                let events = capture.received.lock().unwrap();
                events
                    .iter()
                    .map(|v| v["event_type"].as_str().unwrap_or("?").to_string())
                    .collect()
            };
            return Err(anyhow!(
                "timed out waiting for webhook event '{event_type}'; got {types:?}"
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// PCMU payloads carrying a DTMF digit as AUDIO: `DtmfGenerator` dual tone
/// split into 20 ms frames and G.711-encoded — indistinguishable on the
/// wire from what an old phone/PSTN gateway sends for a key press.
fn dtmf_tone_pcmu_payloads(digit: char, duration_ms: u64) -> Vec<Vec<u8>> {
    let generator = DtmfGenerator::new(SAMPLE_RATE);
    let pcm = generator.generate(digit, duration_ms as u32);
    let mut encoder = audio_codec::create_encoder(audio_codec::CodecType::PCMU);
    pcm.chunks(160)
        .map(|frame| encoder.encode(frame))
        .collect()
}

// ── The scenario ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_inband_dtmf_audio_reaches_app_with_source_marker() -> Result<()> {
    let _ = tracing_subscriber::fmt().try_init();

    // Long interruptible prompt: the key press barges in mid-playback.
    let prompt_wav = std::env::temp_dir().join("rustpbx_e2e_inband_dtmf_prompt.wav");
    write_sine_wav(&prompt_wav, 530.0, 6000)?;

    let provider = start_step_provider(vec![
        serde_json::json!({
            "type": "prompt",
            "file": prompt_wav.to_string_lossy(),
            "interruptible": true,
        }),
        serde_json::json!({"type": "hangup"}),
    ])
    .await;

    let capture = WebhookCapture::start().await;
    let ivr_path: PathBuf = std::env::temp_dir().join(format!(
        "inband-dtmf-ivr-{}.toml",
        portpicker::pick_unused_port().unwrap_or(45000)
    ));
    std::fs::write(&ivr_path, step_ivr_toml(&provider.url))?;

    let server = start_harness(&capture, &ivr_path).await?;
    let mut caller = make_caller_ua(server.proxy_addr);
    caller.start().await?;
    let pump = spawn_caller_pump(caller.clone());

    // Offer WITHOUT telephone-event (PCMU/PCMA only) — this is what arms
    // the leg's in-band detector (`inband_dtmf = "auto"` default).
    let caller_rtp_port = portpicker::pick_unused_port().unwrap_or(33000);
    let offer = create_test_sdp("127.0.0.1", caller_rtp_port, false);
    let dialog = caller.make_call(IVR_NUMBER, Some(offer)).await?;

    // Give the prompt time to start, then press the key AS AUDIO: PCMU RTP
    // carrying the synthesized dual tone from the advertised SDP port.
    sleep(Duration::from_millis(800)).await;
    let answer_sdp = caller
        .get_negotiated_answer_sdp(&dialog)
        .await
        .expect("negotiated answer SDP must be captured for a plain-RTP call");
    let server_media = extract_media_endpoint(&answer_sdp)
        .expect("answer SDP must carry an audio media endpoint");

    let endpoint = RtpEndpoint::bind(caller_rtp_port as u16).await?;
    endpoint.send_pcmu_blocking(
        server_media,
        dtmf_tone_pcmu_payloads(PRESSED_DIGIT, 400),
        20,
    ).await?;

    // 1. App-level proof: the step provider saw the digit (barge-in).
    let provider_dtmf = provider.wait_for_dtmf_event(Duration::from_secs(10)).await?;
    assert_eq!(
        provider_dtmf["digit"].as_str(),
        Some("5"),
        "the app must observe the in-band digit: {provider_dtmf}"
    );
    // 2. RWI proof: the `dtmf` event carries the inband source marker.
    wait_webhook_event(&capture, "dtmf", Duration::from_secs(10)).await?;
    let dtmf_events: Vec<serde_json::Value> = {
        let events = capture.received.lock().unwrap();
        events
            .iter()
            .filter(|b| b["event_type"].as_str() == Some("dtmf"))
            .map(|b| b["event"].clone())
            .collect()
    };
    assert!(
        !dtmf_events.is_empty(),
        "RWI dtmf event must reach the webhook (allow-list includes dtmf)"
    );
    let matched = dtmf_events
        .iter()
        .find(|e| e["digit"].as_str() == Some("5"))
        .expect("a dtmf event for the pressed digit must arrive");
    assert_eq!(
        matched["extra"]["source"].as_str(),
        Some("inband"),
        "audio-recovered digits must be tagged source=inband: {matched}"
    );
    assert_eq!(
        matched["leg_id"].as_str(),
        Some("caller"),
        "in-band digits must carry the source leg: {matched}"
    );

    // 3. Step-trace proof: the app flow recorded the barge-in digit.
    wait_webhook_event(&capture, "call_hangup", Duration::from_secs(20)).await?;
    let trace_has_digit = {
        let events = capture.received.lock().unwrap();
        events.iter().any(|b| {
            b["event_type"].as_str() == Some("ivr_step_trace")
                && b["event"]["trigger"]["detail"]["digit"].as_str() == Some("5")
        })
    };
    assert!(
        trace_has_digit,
        "an ivr_step_trace with trigger.dtmf digit=5 must appear on the webhook"
    );

    endpoint.stop();
    let _ = pump;
    Ok(())
}
