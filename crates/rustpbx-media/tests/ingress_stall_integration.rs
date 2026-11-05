//! Integration tests for the per-leg inbound-RTP stall watcher
//! (`crate::ingress_stall`): real legs over loopback, real RTP flow, then a
//! real silence window — asserting the edges the session forwarder reports
//! as `proxy.ingress_stalled`, the hold-suppression contract, the disabled
//! escape hatch, and the task lifecycle (channel closes on leg `stop()`).

use std::time::Duration;

use rustpbx_media::audio_source::ToneAudioSource;
use rustpbx_media::ingress_stall::{IngressStallEvent, IngressStallKind};
use rustpbx_media::leg::{Leg, LegConfig, LegInner};

/// Hand-rolled 20ms PCMU frame (v=2, pt=0) — fed straight into the leg's
/// RTP transport receive path (`feed_packet`), i.e. a true INBOUND packet
/// for the watched leg, independent of any remote socket.
fn inbound_rtp_bytes(seq: u16, ts: u32, ssrc: u32) -> Vec<u8> {
    let mut b = vec![0x80u8, 0];
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&ts.to_be_bytes());
    b.extend_from_slice(&ssrc.to_be_bytes());
    b.extend_from_slice(&[0u8; 160]);
    b
}

async fn audio_transport(
    leg: &Leg,
) -> std::sync::Arc<rustrtc::transports::rtp::RtpTransport> {
    leg.pc()
        .get_transceivers()
        .into_iter()
        .find(|t| t.kind() == rustrtc::MediaKind::Audio)
        .and_then(|t| t.sender())
        .and_then(|s| s.transport())
        .expect("audio RTP transport")
}

/// Surface the watcher's DEBUG trace (tick decisions, WARN edges) so test
/// failures are diagnosable with `--nocapture`.
fn init_trace() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new("rustpbx_media=debug").unwrap(),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

fn leg_cfg(warn_secs: u64) -> LegConfig {
    let mut cfg = LegConfig::rtp_pcmu();
    cfg.ingress_stall_warn_secs = warn_secs;
    // Production enables quality stats; the tap's last-packet evidence
    // (`last_seq` / `last_peer` in the stall event) rides on it.
    cfg.quality_stats.enabled = true;
    cfg
}

/// Pair `peer` (the watched leg) with a `phone` leg and return the event
/// receiver. Both legs are answered and accepted so RTP flows over loopback.
async fn pair(
    peer_cfg: &LegConfig,
    phone_cfg: &LegConfig,
) -> (Leg, Leg, tokio::sync::broadcast::Receiver<IngressStallEvent>) {
    init_trace();
    let peer = LegInner::new("caller", peer_cfg, None).unwrap();
    let phone = LegInner::new("phone", phone_cfg, None).unwrap();
    let offer = peer.create_offer().await.unwrap();
    let answer = phone.answer(&offer).await.unwrap();
    peer.apply_remote_sdp(&answer, rustrtc::SdpType::Answer).await.unwrap();
    peer.accept();
    phone.accept();
    let rx = peer.subscribe_ingress_stall();
    (peer, phone, rx)
}

#[tokio::test]
async fn stall_edge_fires_once_then_resume_when_media_returns() {
    let (peer, phone, mut rx) = pair(&leg_cfg(1), &leg_cfg(0)).await;

    // ~1.5s of 440Hz into the phone's egress → RTP flows into `peer`.
    phone
        .play_media(
            Box::new(ToneAudioSource::new(440, Duration::from_secs(2), 8000).unwrap()),
            true,
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while peer.pc().received_rtp_packets() < 20 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "caller never received media"
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Phone drops off the wire: caller ingress freezes → the watcher's 1s
    // window completes → exactly one `stalled` edge, with healthy-pipeline
    // evidence (tap count == accepted count) and the last packet's anchor.
    phone.stop();
    let stalled = tokio::time::timeout(Duration::from_secs(6), rx.recv())
        .await
        .expect("stall edge within the 1s threshold + slack")
        .expect("event channel stays open while the leg lives");
    assert_eq!(stalled.kind, IngressStallKind::Stalled);
    assert_eq!(stalled.leg_id, "caller");
    assert!(stalled.silent_secs >= 1, "silent for {:?}", Duration::from_secs(stalled.silent_secs));
    assert!(stalled.lifetime_accepted >= 20);
    assert_eq!(stalled.tap_ingress, stalled.lifetime_accepted, "no internal drops");
    assert!(stalled.last_seq.is_some(), "last-packet anchor present");
    assert!(stalled.last_peer.is_some());
    assert_eq!(stalled.episode, 1);

    // Held silence produces no further edges (single edge per episode).
    let quiet = tokio::time::timeout(Duration::from_millis(2500), rx.recv());
    assert!(
        quiet.await.is_err(),
        "no additional events during a held silence episode"
    );

    // Media returns: inject INBOUND packets straight into the peer's RTP
    // transport (`feed_packet` = the receive path, exactly what a remote
    // socket feed looks like to the watcher). One advance → one `resumed`
    // edge; a second frozen window then re-arms and fires again.
    let transport = audio_transport(&peer).await;
    let addr: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();
    transport
        .feed_packet(bytes::Bytes::from(inbound_rtp_bytes(1, 160, 999)), addr)
        .await;
    let resumed = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("resume edge when media returns")
        .unwrap();
    assert_eq!(resumed.kind, IngressStallKind::Resumed);
    assert!(resumed.silent_secs >= 1);

    peer.stop();
}

#[tokio::test]
async fn hold_suppression_keeps_expected_silence_off_the_wire_events() {
    let (peer, phone, mut rx) = pair(&leg_cfg(1), &leg_cfg(0)).await;

    phone
        .play_media(
            Box::new(ToneAudioSource::new(440, Duration::from_secs(2), 8000).unwrap()),
            true,
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while peer.pc().received_rtp_packets() < 20 {
        assert!(tokio::time::Instant::now() < deadline, "caller never received media");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Hold the peer (what `LegState::Hold` does at session level):
    // `set_ingress_quiet_expected(true)` — inbound silence is now EXPECTED.
    // Then stop the phone. No edge may fire across the pause even though
    // the wire is frozen. (Note: `pause_rtp_timeout` is deliberately NOT
    // the suppression signal — an un-armed RTP timeout is the normal call
    // state.)
    peer.set_ingress_quiet_expected(true);
    phone.stop();
    let quiet = tokio::time::timeout(Duration::from_millis(2500), rx.recv());
    assert!(
        quiet.await.is_err(),
        "held leg must not report an ingress stall"
    );

    // Unhold with the phone still gone: the countdown restarts from now and
    // the stall edge fires only after a fresh full window.
    peer.set_ingress_quiet_expected(false);
    let first = tokio::time::timeout(Duration::from_millis(400), rx.recv());
    assert!(
        first.await.is_err(),
        "no inherited edge right after unhold"
    );
    let stalled = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("fresh-window stall edge after unhold")
        .unwrap();
    assert_eq!(stalled.kind, IngressStallKind::Stalled);

    // Teardown: dropping the leg drops the event sender → the session-side
    // forwarder observes `Closed` and exits (the leak guard).
    drop(peer);
    drop(phone);
    let closed = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(
        matches!(
            closed,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed))
        ),
        "channel must close after the leg is dropped"
    );
}

#[tokio::test]
async fn disabled_watcher_stays_silent() {
    // `ingress_stall_warn_secs: 0` = escape hatch: no task, no events —
    // even across a real freeze after media flowed.
    let (peer, phone, mut rx) = pair(&leg_cfg(0), &leg_cfg(0)).await;
    phone
        .play_media(
            Box::new(ToneAudioSource::new(440, Duration::from_secs(2), 8000).unwrap()),
            true,
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while peer.pc().received_rtp_packets() < 20 {
        assert!(tokio::time::Instant::now() < deadline, "caller never received media");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    phone.stop();
    let quiet = tokio::time::timeout(Duration::from_millis(2500), rx.recv());
    assert!(
        quiet.await.is_err(),
        "disabled watcher must not emit stall edges"
    );
    peer.stop();
}

#[tokio::test]
async fn watcher_channel_closes_when_leg_stops() {
    // Lifecycle: the event channel is owned by the leg — dropping the leg
    // drops the sender and the session-side forwarder observes `Closed` and
    // exits. This is the leak guard: a stuck watcher task would keep the
    // channel open past leg teardown.
    let (peer, _phone, mut rx) = pair(&leg_cfg(1), &leg_cfg(1)).await;
    peer.stop();
    drop(peer);
    let closed = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    match closed {
        Err(_timeout) => panic!("channel must close after leg stop"),
        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {}
        Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
            panic!("unexpected lagged error on an empty channel")
        }
        Ok(Ok(ev)) => panic!("unexpected event after leg stop: {ev:?}"),
    }
}
