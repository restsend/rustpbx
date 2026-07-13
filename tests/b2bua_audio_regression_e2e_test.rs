//! B2BUA audio-forwarding regression across the two anchored-media paths.
//!
//! Two sipbot UAs (alice → PBX → bob) with `MediaProxyMode::All` so the PBX
//! anchors media. We exercise both code paths the VoipBridge work touched:
//!
//! - `test_b2bua_same_codec_fast_path_audio`: alice & bob both PCMU → the RTP
//!   fast-path relay (`AnchoredMediaMode::RelayOnly`) activates. Verifies the
//!   additive `anchored_mode` state and the transport-level relay forward real
//!   non-silent audio bidirectionally.
//! - `test_b2bua_transcode_slow_path_audio`: alice PCMU, bob PCMA → fast-path
//!   ineligible → `wire_both_forwarding_tracks` (the method extracted in the
//!   VoipBridge refactor) wires the ForwardingTrack slow path with transcoding.
//!   Verifies transcoded audio still flows bidirectionally.
//!
//! "Audio forwards correctly" = both legs have RX RTP + non-silent audio
//! quality frames. (Frequency-level Goertzel is covered for the app-bridge
//! path by `audio_content_e2e_test`; the pure B2BUA path can't inject a known
//! tone since `Play` requires an app media_bridge, so we verify real audio
//! energy — the regression-relevant signal that the bridge forwards audio.)

mod helpers;

use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TestPbx, TestPbxInject};
use rustpbx::call::SipUser;
use rustpbx::config::{MediaProxyMode, ProxyConfig, UserBackendConfig};
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;
use uuid::Uuid;

/// Two sipbot UAs through the PBX with `MediaProxyMode::All`. Returns once both
/// legs have bidirectional non-silent audio, or panics after a timeout.
async fn assert_b2bua_bidirectional_audio(
    sip_port: u16,
    alice_codecs: Vec<String>,
    bob_codecs: Vec<String>,
    label: &str,
) {
    let bob_port = portpicker::pick_unused_port().expect("no free bob port");
    let alice_port = portpicker::pick_unused_port().expect("no free alice port");

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

    let bob = TestUa::registered_callee_with_record(
        bob_port,
        1,
        "bob",
        "pass",
        &domain,
        &domain,
        std::env::temp_dir()
            .join(format!("b2bua_{}_bob.wav", label))
            .to_string_lossy()
            .to_string(),
        bob_codecs,
    )
    .await;
    sleep(Duration::from_secs(1)).await; // registration settle

    let target = format!("sip:bob@{}", pbx.sip_host());
    let alice = TestUa::caller_with_target_and_record(
        alice_port,
        "alice",
        target.clone(),
        std::env::temp_dir()
            .join(format!("b2bua_{}_alice.wav", label))
            .to_string_lossy()
            .to_string(),
        alice_codecs,
    )
    .await;
    info!(%target, "[{}] alice calling bob", label);

    // Wait for bidirectional non-silent audio on both legs.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let alice_q = alice.audio_quality_summary();
        let bob_q = bob.audio_quality_summary();
        if alice_q.has_audio() && bob_q.has_audio() {
            info!(
                "[{}] bidirectional audio confirmed: alice total={} silence={}, bob total={} silence={}",
                label, alice_q.total_frames, alice_q.silence_frames, bob_q.total_frames, bob_q.silence_frames
            );
            assert!(alice.has_rtp_rx(), "[{}] alice RX RTP missing", label);
            assert!(bob.has_rtp_rx(), "[{}] bob RX RTP missing", label);
            assert!(alice.has_rtp_tx(), "[{}] alice TX RTP missing", label);
            assert!(bob.has_rtp_tx(), "[{}] bob TX RTP missing", label);
            alice.stop();
            bob.stop();
            pbx.stop();
            return;
        }
        sleep(Duration::from_millis(300)).await;
    }

    // Timeout — fail with diagnostic stats.
    let alice_q = alice.audio_quality_summary();
    let bob_q = bob.audio_quality_summary();
    panic!(
        "[{}] bidirectional audio timeout. alice rx={} tx={} total={} silence={}; bob rx={} tx={} total={} silence={}",
        label,
        alice.has_rtp_rx(), alice.has_rtp_tx(), alice_q.total_frames, alice_q.silence_frames,
        bob.has_rtp_rx(), bob.has_rtp_tx(), bob_q.total_frames, bob_q.silence_frames,
    );
}

/// P0b: same-codec (PCMU/PCMU) B2BUA → RTP fast-path activates. The
/// `anchored_mode = RelayOnly` set in `start_anchored_media_forwarding` is
/// additive and must not break the transport-level relay.
#[tokio::test]
async fn test_b2bua_same_codec_fast_path_audio() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new("rustpbx=info").unwrap(),
        )
        .try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    assert_b2bua_bidirectional_audio(
        sip_port,
        vec!["pcmu".to_string()],
        vec!["pcmu".to_string()],
        "fastpath",
    )
    .await;
    info!("test_b2bua_same_codec_fast_path_audio PASSED");
}

/// P0a: different codecs (PCMU/PCMA) B2BUA → fast-path ineligible →
/// `wire_both_forwarding_tracks` (extracted in the VoipBridge refactor) wires
/// the ForwardingTrack slow path with transcoding.
#[tokio::test]
async fn test_b2bua_transcode_slow_path_audio() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new("rustpbx=info").unwrap(),
        )
        .try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    assert_b2bua_bidirectional_audio(
        sip_port,
        vec!["pcmu".to_string()],
        vec!["pcma".to_string()],
        "transcode",
    )
    .await;
    info!("test_b2bua_transcode_slow_path_audio PASSED");
}

// Keep a reference to Uuid so the crate import isn't flagged unused if the
// helper above is later refactored to drop it.
#[allow(dead_code)]
fn _uuid_used() -> Uuid {
    Uuid::new_v4()
}
