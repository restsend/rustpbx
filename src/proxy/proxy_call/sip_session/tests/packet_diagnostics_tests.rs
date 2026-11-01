use crate::log_capture::LogCapture;
use crate::media::leg::{LegConfig, LegInner};
use std::time::Duration;

// Real leg construction, asynchronous sampler and terminal quality/CDR boundary;
// no bridge is installed, matching an IVR or queue waiting for an agent.
#[test]
fn packet_diagnostics_survive_prebridge_and_stop_at_info() {
    let media = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("packet-log-media")
        .enable_all()
        .build()
        .unwrap();
    let control = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    control.block_on(async {
        let (logs, _guard, _interest) = LogCapture::start_filtered_timestamped("info");
        let mut config = LegConfig::rtp_pcmu();
        config.call_id = Some("diagnostic-full-session-12ab".into());
        config.quality_stats.enabled = true;
        config.quality_stats.sample_interval_ms = 250;
        config.quality_stats.log_interval_ms = 2000;
        let leg = {
            let _media_guard = media.enter();
            LegInner::new("caller-leg", &config, None).unwrap()
        };
        tokio::time::sleep(Duration::from_millis(2150)).await;
        leg.stop();
        leg.stop();
        let rows = logs.text();
        let diagnostics: Vec<_> = rows
            .lines()
            .filter(|line| line.contains("media.packet_stages"))
            .collect();
        assert!(
            diagnostics.len() >= 3,
            "initial, async periodic and final diagnostics missing: {rows}"
        );
        for row in &diagnostics {
            assert!(
                row.contains("session_id=diagnostic-full-session-12ab"),
                "{row}"
            );
            assert!(row.contains("leg_id=caller-leg"), "{row}");
            assert!(
                row.contains("direction=\"peer_to_pbx_and_pbx_to_peer\""),
                "{row}"
            );
            assert!(row.contains("INFO"), "{row}");
        }
        assert_eq!(
            diagnostics
                .iter()
                .filter(|row| row.contains("event=\"final\""))
                .count(),
            1
        );
        assert!(
            diagnostics
                .iter()
                .any(|row| row.contains("event=\"periodic\"") && row.contains("packet-log-media")),
            "periodic output must come from the actual separate media runtime: {rows}"
        );
        let report = serde_json::to_value(leg.quality_report("A")).unwrap();
        assert!(
            report["packetDiagnostics"].is_object(),
            "terminal report must retain diagnostics after PC.close: {report}"
        );
        assert_eq!(report["packetDiagnostics"]["accepted_rtp"], 0);
        assert_eq!(
            report["packetDiagnostics"]["snapshot_scope"],
            "leg_stopping"
        );
        assert_eq!(
            report["packetDiagnostics"]["async_send_results_in_cdr_final"],
            false
        );
        let found = logs.search("diagnostic-full-session-12ab").await;
        assert_eq!(
            found
                .iter()
                .filter(|row| row.contains("media.packet_stages"))
                .count(),
            diagnostics.len()
        );

        // Disabled config must not secretly start another sampler.
        config.call_id = Some("diagnostic-disabled-session".into());
        config.quality_stats.enabled = false;
        let disabled = LegInner::new("disabled-leg", &config, None).unwrap();
        disabled.stop();
        assert!(
            !logs
                .text()
                .lines()
                .any(|row| row.contains("media.packet_stages")
                    && row.contains("diagnostic-disabled-session"))
        );
    });
}

#[tokio::test(flavor = "current_thread")]
async fn packet_diagnostics_real_udp_gap_recovery_and_hold_are_observations() {
    use rustrtc::SdpType;
    use tokio::net::UdpSocket;
    let (logs, _guard, _interest) = LogCapture::start_filtered_timestamped("info");
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = LegConfig::rtp_pcmu();
    config.call_id = Some("node-session-34cd".into());
    config.root_session_id = Some("root-session-56ef".into());
    config.bind_ip = Some("127.0.0.1".into());
    config.quality_stats.enabled = true;
    config.quality_stats.sample_interval_ms = 250;
    config.quality_stats.log_interval_ms = 2000;
    let leg = LegInner::new("remote-caller", &config, None).unwrap();
    let offer = format!(
        "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n",
        socket.local_addr().unwrap().port()
    );
    leg.apply_sdp(&offer, SdpType::Offer).await.unwrap();
    leg.accept();
    let peer = LegInner::new("agent-leg", &config, None).unwrap();
    let mut bridge = crate::media::media_bridge::MediaBridge::new("node-session-34cd");
    bridge.select_pair(leg.clone(), peer.clone()).await.unwrap();
    let destination: std::net::SocketAddr = leg.advertised_addr().unwrap().parse().unwrap();
    // A datagram can reach UDP and the RTP demux while failing RTP parsing.
    socket.send_to(&[0x80, 0], destination).await.unwrap();
    for sequence in 0..8u16 {
        let mut packet = vec![0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7];
        packet[2..4].copy_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&[0xff; 160]);
        socket.send_to(&packet, destination).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(5800)).await;
    for sequence in 8..16u16 {
        let mut packet = vec![0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7];
        packet[2..4].copy_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&[0xff; 160]);
        socket.send_to(&packet, destination).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(350)).await;
    let before_hold = logs.text();
    let stages: Vec<_> = before_hold
        .lines()
        .filter(|r| r.contains("media.packet_stages") && r.contains("leg_id=remote-caller"))
        .collect();
    assert_eq!(
        stages
            .iter()
            .filter(|r| r.contains("rtp_accepted:no_growth"))
            .count(),
        1,
        "{before_hold}"
    );
    assert_eq!(
        stages
            .iter()
            .filter(|r| r.contains("rtp_accepted:resumed"))
            .count(),
        1,
        "{before_hold}"
    );
    for row in &stages {
        assert!(row.contains("session_id=node-session-34cd"), "{row}");
        assert!(row.contains("root_session_id=root-session-56ef"), "{row}");
        assert!(
            row.contains("stage_observation_only_dtx_or_hold_may_be_silent"),
            "{row}"
        );
    }
    assert!(
        stages
            .iter()
            .filter(|r| r.contains("rtp_accepted:no_growth"))
            .all(|r| r.contains("agent-leg")),
        "gap snapshot must include peer leg"
    );
    assert!(
        stages.len() < 12,
        "per-packet or repeated idle logs: {}",
        stages.len()
    );
    leg.set_app_paused(true);
    tokio::time::sleep(Duration::from_millis(5500)).await;
    leg.stop();
    bridge.close();
    let after = logs.text();
    let added = &after[before_hold.len()..];
    assert!(
        !added.contains(":no_growth"),
        "hold/end must not create a new gap event: {added}"
    );
    let report = serde_json::to_value(leg.quality_report("A")).unwrap();
    let diagnostics = &report["packetDiagnostics"];
    assert_eq!(diagnostics["accepted_rtp"], 16);
    assert_eq!(diagnostics["stages"]["ice"]["udp_read"]["packets"], 17);
    assert_eq!(
        diagnostics["stages"]["transports"][0]["rtp_parse_errors"]["packets"],
        1
    );
    assert!(
        diagnostics["stages"]["transports"][0]["rtp_accepted"]["last_age_ms"]
            .as_u64()
            .unwrap()
            > 5000
    );
    // Save actual emitted records for the implementation handoff, not synthetic examples.
    std::fs::write(
        std::env::temp_dir().join("pbx-packet-diagnostics-test.log"),
        &after,
    )
    .unwrap();
    let root_rows = logs.search("root-session-56ef").await;
    assert_eq!(
        root_rows
            .iter()
            .filter(|r| r.contains("media.packet_stages"))
            .count(),
        after
            .lines()
            .filter(|r| r.contains("media.packet_stages"))
            .count()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn packet_diagnostics_udp_arrival_is_not_srtp_acceptance() {
    use rustrtc::{SdpType, TransportMode};
    use tokio::net::UdpSocket;
    let (logs, _guard, _interest) = LogCapture::start_filtered_timestamped("info");
    let mut config = LegConfig::rtp_pcmu();
    config.transport = TransportMode::Srtp;
    config.call_id = Some("encrypted-node-session".into());
    config.root_session_id = Some("encrypted-root-session".into());
    config.bind_ip = Some("127.0.0.1".into());
    config.quality_stats.enabled = true;
    config.quality_stats.sample_interval_ms = 250;
    config.quality_stats.log_interval_ms = 2000;
    let a = LegInner::new("encrypted-receiver", &config, None).unwrap();
    let b = LegInner::new("encrypted-sender", &config, None).unwrap();
    let offer = a.create_offer().await.unwrap();
    let answer = b.apply_sdp(&offer, SdpType::Offer).await.unwrap();
    a.apply_sdp(&answer, SdpType::Answer).await.unwrap();
    a.accept();
    b.accept();
    let destination: std::net::SocketAddr = a.advertised_addr().unwrap().parse().unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut unauthenticated = vec![0x80, 0, 0, 1, 0, 0, 0, 160, 0, 0, 0, 42];
    unauthenticated.extend_from_slice(&[0; 160]);
    socket.send_to(&unauthenticated, destination).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let value = serde_json::to_value(a.quality_report("A")).unwrap();
            if value["packetDiagnostics"]["stages"]["transports"][0]["srtp_unprotect_errors"]["packets"] == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("invalid encrypted datagram must reach actual UDP and SRTP boundaries");
    let value = serde_json::to_value(a.quality_report("A")).unwrap();
    assert_eq!(value["packetDiagnostics"]["accepted_rtp"], 0);
    assert_eq!(
        value["packetDiagnostics"]["stages"]["ice"]["udp_read"]["packets"],
        1
    );
    assert_eq!(
        value["packetDiagnostics"]["stages"]["transports"][0]["conn"]["rtp"]["packets"],
        1
    );
    b.play(
        Box::new(
            crate::media::audio_source::ToneAudioSource::new(440, Duration::from_secs(1), 8000)
                .unwrap(),
        ),
        true,
        None,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while a.pc().received_rtp_packets() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("valid SRTP must still pass after authentication rejection");
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.stop();
    b.stop();
    let value = serde_json::to_value(a.quality_report("A")).unwrap();
    assert!(value["packetDiagnostics"]["accepted_rtp"].as_u64().unwrap() > 0);
    assert_eq!(
        value["packetDiagnostics"]["stages"]["transports"][0]["srtp_unprotect_errors"]["packets"],
        1
    );
    let text = logs.text();
    assert!(text.lines().any(|row| row.contains("media.packet_stages")
        && row.contains("session_id=encrypted-node-session")
        && row.contains("leg_id=encrypted-receiver")
        && row.contains("srtp_unprotect_errors")));
}

// Both real PC transports are created after the configured leg sampler. The
// remote sendonly direction is an ordinary hold contract, not a send failure.
async fn diagnostic_bridge_with_held_target() -> (
    std::sync::Arc<LegInner>,
    std::sync::Arc<LegInner>,
    crate::media::media_bridge::MediaBridge,
    tokio::net::UdpSocket,
) {
    use rustrtc::SdpType;
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut config = LegConfig::rtp_pcmu();
    config.call_id = Some("hold-node-full-session".into());
    config.root_session_id = Some("hold-root-full-session".into());
    config.bind_ip = Some("127.0.0.1".into());
    config.quality_stats.enabled = true;
    config.quality_stats.sample_interval_ms = 250;
    config.quality_stats.log_interval_ms = 5000;
    let a = LegInner::new("source-leg", &config, None).unwrap();
    let b = LegInner::new("held-leg", &config, None).unwrap();
    for (leg, direction) in [(&a, "sendrecv"), (&b, "sendonly")] {
        let offer = format!(
            "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na={direction}\r\n",
            socket.local_addr().unwrap().port()
        );
        leg.apply_sdp(&offer, SdpType::Offer).await.unwrap();
        leg.pc()
            .wait_for_rtp_transport_ready(Duration::from_secs(2))
            .await
            .unwrap();
        leg.accept();
    }
    let transport = |leg: &LegInner| {
        leg.pc()
            .get_transceivers()
            .into_iter()
            .find_map(|t| t.sender().and_then(|s| s.transport()))
            .unwrap()
    };
    let mut bridge = crate::media::media_bridge::MediaBridge::new("hold-node-full-session");
    bridge.select_pair(a.clone(), b.clone()).await.unwrap();
    transport(&a).bridge_rewrite_to(transport(&b), Default::default());
    (a, b, bridge, socket)
}

#[tokio::test(flavor = "current_thread")]
async fn packet_diagnostics_real_hold_direction_drop_is_not_stage_error() {
    let (logs, _guard, _interest) = LogCapture::start_filtered_timestamped("info");
    let (a, b, mut bridge, socket) = diagnostic_bridge_with_held_target().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = logs.text().len();
    let destination: std::net::SocketAddr = a.advertised_addr().unwrap().parse().unwrap();
    for sequence in 0..8 {
        let packet = rustrtc::rtp::RtpPacket::new(
            rustrtc::rtp::RtpHeader::new(0, sequence, 160, 7),
            vec![0xff; 160],
        )
        .marshal()
        .unwrap();
        socket.send_to(&packet, destination).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(a.pc().received_rtp_packets(), 8);
    // Configured before SDP, installed on the actual later-created PC transport.
    assert!(
        a.pc().packet_diagnostics().transports[0]
            .conn
            .deferred_send_observer_installed
    );
    assert!(
        b.pc().packet_diagnostics().transports[0]
            .conn
            .deferred_send_observer_installed
    );
    let held = serde_json::to_value(b.quality_report("B")).unwrap();
    assert_eq!(
        held["packetDiagnostics"]["stages"]["transports"][0]["send_direction_dropped"]["packets"],
        8
    );
    let after = logs.text();
    assert!(
        !after[before..]
            .lines()
            .any(|row| row.contains("media.packet_stages")
                && row.contains("stage_error:first_in_burst")),
        "normal hold was an error: {after}"
    );
    a.stop();
    b.stop();
    bridge.close();
}

#[tokio::test(flavor = "current_thread")]
async fn packet_diagnostics_rtcp_closed_listener_triggers_peer_snapshot() {
    let (logs, _guard, _interest) = LogCapture::start_filtered_timestamped("info");
    let (a, b, mut bridge, socket) = diagnostic_bridge_with_held_target().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let transport = a
        .pc()
        .get_transceivers()
        .into_iter()
        .find_map(|t| t.sender().and_then(|s| s.transport()))
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    transport.register_rtcp_listener(tx);
    let before = logs.text().len();
    let destination: std::net::SocketAddr = a.advertised_addr().unwrap().parse().unwrap();
    socket
        .send_to(&[0x80, 201, 0, 1, 0, 0, 0, 7], destination)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        transport.packet_diagnostics().rtcp_listener_closed.packets,
        1
    );
    let after = logs.text();
    assert!(
        after[before..]
            .lines()
            .any(|row| row.contains("media.packet_stages")
                && row.contains("leg_id=source-leg")
                && row.contains("stage_error:first_in_burst")
                && row.contains("held-leg")
                && row.contains("rtcp_listener_closed")),
        "RTCP failure needs immediate context for both legs: {after}"
    );
    a.stop();
    b.stop();
    bridge.close();
}
