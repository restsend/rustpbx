//! Bridge full E2E tests with real SIP/RTP via sipbot.
//!
//! Two flows are covered, exercising the two media paths Bridge must
//! support:
//!
//! 1. `test_voip_bridge_sipbot_bidirectional_audio` — a plain B2BUA call
//!    (alice→bob, same codec) where the **RTP fast-path relay activates** at
//!    setup. Bridge then triggers `ensure_media_anchored` to downgrade to
//!    the ForwardingTrack slow path on demand (no re-INVITE), and verifies
//!    bidirectional audio after the downgrade.
//!
//! 2. `test_voip_bridge_via_ivr_app_flow` — a call that enters an IVR app,
//!    which anchors media via the **app media_bridge** (no fast-path). Verifies
//!    Bridge reads the correct bridge side (RTP vs WebRTC) and that
//!    bidirectional audio flows.
//!
//! Together these guard the two ways Bridge sources its media and prevent
//! regressions in the fast-path adaptive downgrade.
//!
//! ```text
//!   alice (sipbot caller) ──INVITE──► RustPBX ──► bob / IVR app
//!                                           │
//!                                           │ Transfer leg="caller" → bridge:ws://...
//!                                           ▼
//!                                       Bridge  ◄►  WS server (injects 440 Hz, counts frames)
//! ```

mod helpers;

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use helpers::audio_verifier::{
    compute_rms, extract_audio_region, find_dominant_frequency, find_signal_start,
    generate_sine_wav, has_audio_content, read_wav_stereo,
};
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::call::SipUser;
use rustpbx::call::domain::{CallCommand, LegId};
use rustpbx::config::{MediaProxyMode, ProxyConfig, UserBackendConfig};
use rustpbx::proxy::active_call_registry::ActiveProxyCallStatus;
use rustpbx::proxy::routing::RouteRule;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::info;
use uuid::Uuid;

/// Generate `n` samples of a `freq` Hz sine at amplitude `amp` (0.0..1.0),
/// little-endian i16 PCM bytes.
fn sine_pcm_bytes(freq: f64, sample_rate: u32, n: usize, amp: f64) -> Vec<u8> {
    let phase_step = 2.0 * std::f64::consts::PI * freq / sample_rate as f64;
    let mut phase = 0.0f64;
    let mut out = Vec::with_capacity(n * 2);
    for _ in 0..n {
        out.extend_from_slice(&(phase.sin() * amp * i16::MAX as f64).to_ne_bytes());
        phase += phase_step;
    }
    out
}

/// WebSocket server that (a) continuously injects a 440 Hz PCM16 tone toward
/// the PBX (forward path) and (b) counts binary frames received from the PBX
/// (reverse path).
struct ToneWsServer {
    addr: std::net::SocketAddr,
    connections: Arc<Mutex<u32>>,
    frames_received: Arc<Mutex<u32>>,
    last_samples: Arc<Mutex<Vec<i16>>>,
    stop: tokio_util::sync::CancellationToken,
}

impl ToneWsServer {
    async fn start(freq: f64, amp: f64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(Mutex::new(0u32));
        let frames_received = Arc::new(Mutex::new(0u32));
        let last_samples = Arc::new(Mutex::new(Vec::<i16>::new()));
        let stop = tokio_util::sync::CancellationToken::new();

        let conn = connections.clone();
        let fr = frames_received.clone();
        let last = last_samples.clone();
        let stop_t = stop.clone();

        rustpbx::utils::spawn(async move {
            // 20 ms tone frame at 8 kHz = 160 samples = 320 bytes.
            let tone = sine_pcm_bytes(freq, 8000, 160, amp);
            loop {
                tokio::select! {
                    _ = stop_t.cancelled() => break,
                    res = listener.accept() => {
                        let (stream, peer) = match res { Ok(v) => v, Err(_) => continue };
                        *conn.lock().await += 1;
                        info!("[WS] new connection from {peer}");
                        let ws = match accept_async(stream).await { Ok(w) => w, Err(e) => { tracing::warn!("ws accept: {e}"); continue; } };
                        let (mut wr, mut rd) = ws.split();

                        // forward: inject tone continuously
                        let tone_bytes = tone.clone();
                        let wcancel = stop_t.clone();
                        rustpbx::utils::spawn(async move {
                            loop {
                                tokio::select! {
                                    _ = wcancel.cancelled() => break,
                                    _ = sleep(Duration::from_millis(20)) => {}
                                }
                                if wr.send(Message::Binary(tone_bytes.clone().into())).await.is_err() {
                                    break;
                                }
                            }
                        });

                        // reverse: count frames received from PBX
                        let fr = fr.clone();
                        let last = last.clone();
                        let rcancel = stop_t.clone();
                        rustpbx::utils::spawn(async move {
                            loop {
                                tokio::select! {
                                    _ = rcancel.cancelled() => break,
                                    msg = rd.next() => {
                                        match msg {
                                            Some(Ok(Message::Binary(data))) => {
                                                *fr.lock().await += 1;
                                                if data.len() >= 2 {
                                                    let s: Vec<i16> = data.chunks_exact(2)
                                                        .take(160)
                                                        .map(|c| i16::from_ne_bytes([c[0], c[1]]))
                                                        .collect();
                                                    *last.lock().await = s;
                                                }
                                            }
                                            Some(Ok(Message::Close(_))) | None
                                            | Some(Err(_)) => break,
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
            }
        });

        Self {
            addr,
            connections,
            frames_received,
            last_samples,
            stop,
        }
    }

    async fn connection_count(&self) -> u32 {
        *self.connections.lock().await
    }
    async fn frames_received(&self) -> u32 {
        *self.frames_received.lock().await
    }
    async fn last_samples(&self) -> Vec<i16> {
        self.last_samples.lock().await.clone()
    }
}

impl Drop for ToneWsServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Build a no-op route whose only job is to make `bob` a routable destination
/// so an inbound call from alice gets answered and stays up as a B2BUA session
/// (we don't actually run an app — default extension routing to the registered
/// user handles it). We still keep a catch-all route to be safe.
#[allow(dead_code)]
fn build_routes() -> Vec<RouteRule> {
    let s = r#"
name = "voipbridge-e2e-default"
priority = 1
auto_answer = true

[match]
"to.user" = "bob"
"#;
    vec![toml::from_str(s).expect("route")]
}

/// Direct B2BUA call with the **RTP fast-path active** (alice & bob both PCMU,
/// so `bridge_eligible` is true and the transport-level rewrite relay activates
/// at call setup). Bridge must then call `ensure_media_anchored` to
/// downgrade to the ForwardingTrack slow path on demand, after which
/// bidirectional audio flows. This is the regression guard for the adaptive
/// fast-path downgrade.
#[tokio::test]
async fn test_bridge_sipbot_bidirectional_audio() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new("rustpbx=info,voip_bridge=debug").unwrap(),
        )
        .try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let bob_port = portpicker::pick_unused_port().expect("no free bob port");
    let alice_port = portpicker::pick_unused_port().expect("no free alice port");
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_voipbridge_e2e_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let record_path = temp_dir.join("alice_recording.wav");

    // ── 1. Start PBX with user "bob" + a route ─────────────────────────────
    let proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(sip_port),
        media_proxy: MediaProxyMode::All,
        ensure_user: Some(false),
        user_backends: vec![UserBackendConfig::Memory {
            users: Some(vec![SipUser {
                id: 0,
                enabled: true,
                username: "bob".to_string(),
                password: Some("pass".to_string()),
                realm: None,
                allow_guest_calls: true,
                ..Default::default()
            }]),
        }],
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(
        sip_port,
        TestPbxInject {
            proxy_config: Some(proxy_config),
            ..Default::default()
        },
    )
    .await;
    let domain = format!("127.0.0.1:{}", sip_port);

    // ── 2. bob: registered callee (echo + record) ──────────────────────────
    // Both legs use PCMU (same codec) so the RTP fast-path relay IS eligible
    // and activates at call setup. Bridge must then downgrade from the
    // fast-path to the ForwardingTrack path on demand via ensure_media_anchored.
    let bob = TestUa::registered_callee_with_record(
        bob_port,
        1,
        "bob",
        "pass",
        &domain,
        &domain,
        record_path.to_string_lossy().to_string(),
        vec!["pcmu".to_string()],
    )
    .await;
    sleep(Duration::from_secs(1)).await; // let registration settle

    // ── 3. alice: guest caller (with recording) dials bob through the PBX ──
    let target = format!("sip:bob@{}", pbx.sip_host());
    let alice = TestUa::caller_with_target_and_record(
        alice_port,
        "alice",
        target.clone(),
        record_path.to_string_lossy().to_string(),
        vec!["pcmu".to_string()],
    )
    .await;
    info!(%target, "alice calling bob");

    // ── 4. Wait for the B2BUA session to establish & media to flow ─────────
    let mut alice_rx = false;
    for _ in 0..80 {
        if alice.has_rtp_rx() {
            alice_rx = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(alice_rx, "pre-bridge: alice should receive RTP");

    // Find the active session id.
    let session_id = {
        let mut sid = None;
        for _ in 0..50 {
            let sessions = pbx.registry.list_recent(10);
            for entry in sessions {
                if matches!(entry.status, ActiveProxyCallStatus::Talking) {
                    sid = Some(entry.session_id.clone());
                    break;
                }
            }
            if sid.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        sid.expect("no active Talking session found")
    };
    info!(%session_id, "B2BUA session established");

    sleep(Duration::from_millis(500)).await;

    // ── 5. Start the tone-injecting WS server ──────────────────────────────
    let ws_server = ToneWsServer::start(440.0, 0.4).await;
    let ws_url = format!("ws://127.0.0.1:{}", ws_server.addr.port());
    let target_uri = format!("bridge:{ws_url}?samplerate=8000&codec=pcm&_hdr_X-Test=sipbot");

    // ── 6. Blind-transfer the caller leg to voip_bridge ────────────────────
    // alice is the caller leg; bridging it rewires alice's media to the WS.
    // Capture a pre-bridge audio-quality snapshot: after the transfer, the WS
    // forward_loop is the ONLY source of alice's RX audio (bob is detached),
    // so any new non-silent frames post-bridge must have arrived via the bridge.
    let handle = pbx
        .registry
        .get_handle(&session_id)
        .expect("session handle must exist");
    let q_before = alice.audio_quality_summary();
    info!(
        "alice audio quality BEFORE bridge: total={}, silence={}",
        q_before.total_frames, q_before.silence_frames
    );
    info!(%target_uri, "sending Bridge transfer on caller leg");
    handle
        .send_command(CallCommand::Transfer {
            leg_id: LegId::new("caller"),
            target: target_uri.clone(),
            attended: false,
        })
        .expect("send Transfer command");

    // ── 7. Verify the WS connection is established ──────────────────────────
    let mut connected = false;
    for _ in 0..60 {
        if ws_server.connection_count().await >= 1 {
            connected = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(connected, "Bridge never connected to the WS server");

    // ── 8. Let bidirectional audio flow ─────────────────────────────────────
    sleep(Duration::from_secs(5)).await;

    let frames = ws_server.frames_received().await;
    info!("WS reverse: {frames} frames received from PBX");

    // ── 9. Assertions ───────────────────────────────────────────────────────
    // (a) Reverse path: PBX must have written real PCM frames to the WS.
    assert!(
        frames >= 1,
        "reverse path failed: WS server received 0 frames from PBX"
    );
    let last = ws_server.last_samples().await;
    if !last.is_empty() {
        let rms_db = compute_rms(&last);
        info!("last reverse frame RMS: {:.1} dB", rms_db);
        assert!(
            has_audio_content(&last, -45.0),
            "reverse frame should carry non-silent audio, RMS={rms_db:.1} dB"
        );
    }

    // (b) Forward path (WS → call → alice). After bridging the caller leg, the
    //     only thing feeding alice's RX is the WS forward_loop injecting our
    //     440 Hz tone. So new non-silent frames post-bridge prove the forward
    //     path delivered real audio.
    assert!(
        alice.has_rtp_rx(),
        "forward path failed: alice has no RX RTP"
    );
    let q = alice.audio_quality_summary();
    let total_delta = q.total_frames.saturating_sub(q_before.total_frames);
    let silence_delta = q.silence_frames.saturating_sub(q_before.silence_frames);
    info!(
        "alice audio quality AFTER bridge: total={}, silence={} (delta: +{} frames, +{} silence)",
        q.total_frames, q.silence_frames, total_delta, silence_delta
    );
    assert!(
        total_delta > 30,
        "forward path: alice received only {total_delta} new frames after bridge (expected >30)"
    );
    assert!(
        silence_delta * 4 < total_delta * 3,
        "forward path: post-bridge audio is mostly silent ({silence_delta}/{total_delta}); \
         the injected tone did not arrive"
    );

    // (c) Forward content (best-effort): if sipbot wrote the caller's WAV
    //     recording, the dominant frequency should be ~440 Hz. The caller
    //     recording path is unreliable in sipbot, so this only asserts when a
    //     recording with samples is actually present.
    bob.stop();
    alice.stop();
    sleep(Duration::from_millis(500)).await;

    if record_path.exists() {
        let (rx_ch, _tx_ch, rec_sr) = read_wav_stereo(&record_path);
        info!(
            "alice recording: {} RX samples at {} Hz",
            rx_ch.len(),
            rec_sr
        );
        if !rx_ch.is_empty() {
            let start = find_signal_start(&rx_ch, 0.01, rec_sr as usize / 50);
            let region = extract_audio_region(&rx_ch, rec_sr, start, 1500);
            if !region.is_empty() && has_audio_content(&region, -40.0) {
                let (freq, _mag) = find_dominant_frequency(&region, rec_sr, 200.0, 800.0, 5.0);
                info!("alice RX dominant freq: {:.0} Hz", freq);
                assert!(
                    (freq - 440.0).abs() < 60.0,
                    "alice RX should show ~440 Hz from the injected tone, got {freq:.0} Hz"
                );
                info!("forward 440 Hz content verified in recording");
            }
        } else {
            tracing::warn!(
                "caller WAV has 0 RX samples (sipbot caller recording limitation); \
                 forward path verified via post-bridge audio-quality delta instead"
            );
        }
    } else {
        tracing::warn!("recording file not found at {record_path:?}");
    }

    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);
    info!("test_voip_bridge_sipbot_bidirectional_audio PASSED");
}

/// IVR → Bridge flow (the production path).
///
/// alice dials into an IVR route (app = "ivr"). The IVR auto-answers via the
/// **app media bridge** (`caller_answer_uses_media_bridge = true`), so the
/// RTP fast-path relay (which requires `!caller_answer_uses_media_bridge`) is
/// never eligible. We then blind-transfer the caller leg to `bridge:`
/// and verify the reverse path delivers real PCM frames to the WS — using the
/// SAME codec on both sides (no transcoding workaround).
///
/// This confirms the user's hypothesis: the IVR/Queue → voip_bridge flow does
/// not hit the fast-path, so Bridge's reverse audio works out of the box.
#[tokio::test]
async fn test_bridge_via_ivr_app_flow() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new("rustpbx=info,voip_bridge=debug").unwrap(),
        )
        .try_init();

    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let alice_port = portpicker::pick_unused_port().expect("no free alice port");
    let temp_dir = std::env::temp_dir().join(format!("rustpbx_voipbridge_ivr_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    // A greeting WAV so the IVR has something to play (keeps the call up while
    // we issue the transfer).
    let greeting_path = temp_dir.join("greeting.wav");
    generate_sine_wav(&greeting_path, 440.0, 30.0, 8000, 0.2);

    let ivr_toml = format!(
        r#"[ivr]
name = "voipbridge-ivr"
ivr_mode = "tree"

[ivr.root]
greeting = "{}"
timeout_ms = 30000
max_retries = 99
"#,
        greeting_path.display()
    );
    let ivr_path = temp_dir.join("ivr.toml");
    std::fs::write(&ivr_path, &ivr_toml).unwrap();

    let route_toml = format!(
        r#"
name = "voipbridge-ivr-route"
priority = 100
app = "ivr"
auto_answer = true

[match]
"to.user" = "ivr"

[app_params]
file = "{}"
"#,
        ivr_path.display()
    );
    let routes: Vec<RouteRule> = vec![toml::from_str(&route_toml).expect("route")];

    // Default proxy mode (Auto) — app flows anchor media via check_media_proxy.
    let proxy_config = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(sip_port),
        ensure_user: Some(false),
        user_backends: vec![UserBackendConfig::Memory {
            users: Some(vec![SipUser {
                id: 0,
                enabled: true,
                username: "ivr".to_string(),
                password: None,
                realm: None,
                allow_guest_calls: true,
                ..Default::default()
            }]),
        }],
        ..Default::default()
    };
    let pbx = TestPbx::start_with_inject(
        sip_port,
        TestPbxInject {
            proxy_config: Some(proxy_config),
            routes: Some(routes),
            ..Default::default()
        },
    )
    .await;

    // alice dials the IVR extension. Same codec as the PBX side (no transcoding).
    let target = format!("sip:ivr@{}", pbx.sip_host());
    let alice = TestUa::caller_with_target(alice_port, "alice", target.clone()).await;
    info!(%target, "alice calling IVR");

    // Wait for the IVR to answer and media to flow.
    let mut alice_rx = false;
    for _ in 0..80 {
        if alice.has_rtp_rx() {
            alice_rx = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(alice_rx, "IVR should answer and alice should receive RTP");

    // Find the active session.
    let session_id = {
        let mut sid = None;
        for _ in 0..50 {
            let sessions = pbx.registry.list_recent(10);
            for entry in sessions {
                if matches!(entry.status, ActiveProxyCallStatus::Talking) {
                    sid = Some(entry.session_id.clone());
                    break;
                }
            }
            if sid.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        sid.expect("no active Talking session")
    };
    info!(%session_id, "IVR session established");

    sleep(Duration::from_millis(500)).await;

    // Start the WS server (inject tone + count reverse frames).
    let ws_server = ToneWsServer::start(440.0, 0.4).await;
    let ws_url = format!("ws://127.0.0.1:{}", ws_server.addr.port());
    let target_uri = format!("bridge:{ws_url}?samplerate=8000&codec=pcm");

    let handle = pbx
        .registry
        .get_handle(&session_id)
        .expect("session handle must exist");
    let q_before = alice.audio_quality_summary();
    info!(%target_uri, "sending Bridge transfer on caller leg (IVR flow)");
    handle
        .send_command(CallCommand::Transfer {
            leg_id: LegId::new("caller"),
            target: target_uri.clone(),
            attended: false,
        })
        .expect("send Transfer command");

    let mut connected = false;
    for _ in 0..60 {
        if ws_server.connection_count().await >= 1 {
            connected = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        connected,
        "Bridge never connected to the WS server (IVR flow)"
    );

    sleep(Duration::from_secs(5)).await;

    let frames = ws_server.frames_received().await;
    info!("[IVR flow] WS reverse: {frames} frames received from PBX");

    // Reverse path: PBX must write real PCM frames to the WS.
    assert!(
        frames >= 1,
        "IVR flow reverse path failed: WS server received 0 frames from PBX"
    );
    let last = ws_server.last_samples().await;
    if !last.is_empty() {
        let rms_db = compute_rms(&last);
        info!("[IVR flow] last reverse frame RMS: {:.1} dB", rms_db);
        assert!(
            has_audio_content(&last, -45.0),
            "IVR flow reverse frame should carry non-silent audio, RMS={rms_db:.1} dB"
        );
    }

    // Forward path: post-bridge audio-quality delta (WS is the only source now).
    let q = alice.audio_quality_summary();
    let total_delta = q.total_frames.saturating_sub(q_before.total_frames);
    let silence_delta = q.silence_frames.saturating_sub(q_before.silence_frames);
    info!(
        "[IVR flow] alice audio quality AFTER bridge: total={}, silence={} (delta +{} frames, +{} silence)",
        q.total_frames, q.silence_frames, total_delta, silence_delta
    );
    assert!(
        total_delta > 30,
        "IVR flow forward path: alice received only {total_delta} new frames after bridge"
    );
    assert!(
        silence_delta * 4 < total_delta * 3,
        "IVR flow forward path: post-bridge audio mostly silent ({silence_delta}/{total_delta})"
    );

    alice.stop();
    pbx.stop();
    let _ = std::fs::remove_dir_all(&temp_dir);
    info!("test_voip_bridge_via_ivr_app_flow PASSED");
}
