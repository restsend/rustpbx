use crate::log_capture::LogCapture;
use crate::media::leg::{LegConfig, LegInner};
use std::time::Duration;

async fn feed_ingress_review_packet(
    socket: &tokio::net::UdpSocket,
    leg: &crate::media::leg::Leg,
    seq: u16,
) {
    let before = leg.pc().received_rtp_packets();
    let mut packet = vec![0x80, 0];
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&(u32::from(seq) * 160).to_be_bytes());
    packet.extend_from_slice(&1234u32.to_be_bytes());
    packet.extend_from_slice(&[0xff; 160]);
    let advertised: std::net::SocketAddr = leg.advertised_addr().unwrap().parse().unwrap();
    socket.send_to(&packet, advertised).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while leg.pc().received_rtp_packets() == before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("UDP packet must reach accepted RTP before testing a stall");
}

async fn assert_ingress_review_reaches_session(name: &str) {
    use super::*;
    use crate::call::{DialDirection, Dialplan, Location, MediaConfig};
    use crate::proxy::tests::common::create_test_request;
    use crate::proxy::tests::test_sip_session_regressions::build_session_with_cmd_rx;

    let request = create_test_request(
        rsipstack::sip::Method::Invite,
        "customer",
        None,
        "example.test",
        None,
    );
    let plan = Dialplan::new(
        "ingress-review-session".into(),
        request,
        DialDirection::Inbound,
    )
    .with_media(
        MediaConfig::new()
            .with_bind_ip(Some("127.0.0.1".into()))
            .with_ingress_stall_warn_secs(Some(1)),
    );
    let (mut session, _handle, mut commands) = build_session_with_cmd_rx(plan).await;
    let id = LegId::from(name);
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sdp = format!(
        "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n",
        socket.local_addr().unwrap().port(),
    );
    if name == "callee" {
        session
            .ensure_media_leg(LegSide::B, &sdp, MediaTransport::Rtp)
            .await
            .unwrap();
    } else {
        session
            .legs
            .insert(id.clone(), crate::call::domain::Leg::new(id.clone()));
        let location = Location {
            aor: format!("sip:agent@{}", socket.local_addr().unwrap())
                .parse()
                .unwrap(),
            ..Default::default()
        };
        session.initiate_sip_leg(&id, location, None).await.unwrap();
    }
    let peer = session.media_leg(&id).unwrap();
    peer.apply_sdp(
        &sdp,
        if name == "callee" {
            rustrtc::SdpType::Offer
        } else {
            rustrtc::SdpType::Answer
        },
    )
    .await
    .unwrap();
    peer.accept();
    feed_ingress_review_packet(&socket, &peer, 1).await;
    let detail = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let command = commands.recv().await.unwrap();
            if let CallCommand::ReportCallError { code, detail, .. } = &command {
                assert_eq!(code, "proxy.ingress_stalled");
                let detail = detail.clone().unwrap();
                assert!(session.execute_command(command, None).await.success);
                let record = session.record_snapshot();
                assert!(
                    record.metadata["trace"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["code"] == "proxy.ingress_stalled"
                            && entry["detail"]["leg_id"] == name)
                );
                break detail;
            }
        }
    })
    .await
    .expect("the production leg attachment must forward its stall");
    assert_eq!(detail["leg_id"], name);
    session.cancel_token.cancel();
    peer.stop();
}

#[tokio::test]
async fn vendor_review_regression_callee_ingress_reaches_session() {
    assert_ingress_review_reaches_session("callee").await;
}

#[tokio::test]
async fn vendor_review_regression_dynamic_ingress_reaches_session() {
    assert_ingress_review_reaches_session("queue-agent").await;
}

#[tokio::test]
async fn vendor_review_regression_ingress_logs_keep_full_session_identity() {
    let (logs, _guard, _interest) = LogCapture::start_filtered("debug");
    let mut config = LegConfig::rtp_pcmu();
    config.call_id = Some("ingress-full-session-1234567890abcdef".into());
    config.ingress_stall_warn_secs = 1;
    config.bind_ip = Some("127.0.0.1".into());
    let leg = LegInner::new("callee", &config, None).unwrap();
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let offer = format!(
        "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n",
        socket.local_addr().unwrap().port(),
    );
    leg.apply_sdp(&offer, rustrtc::SdpType::Offer)
        .await
        .unwrap();
    leg.accept();
    let mut events = leg.subscribe_ingress_stall();
    feed_ingress_review_packet(&socket, &leg, 1).await;
    let stalled = tokio::time::timeout(Duration::from_secs(4), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stalled.kind,
        crate::media::ingress_stall::IngressStallKind::Stalled
    );
    feed_ingress_review_packet(&socket, &leg, 2).await;
    let resumed = tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resumed.kind,
        crate::media::ingress_stall::IngressStallKind::Resumed
    );
    leg.stop();
    let output = logs.text();
    let rows: Vec<_> = output
        .lines()
        .filter(|line| {
            line.contains("ingress watch tick")
                || line.contains("leg ingress stalled")
                || line.contains("leg ingress resumed")
        })
        .collect();
    assert!(rows.iter().any(|line| line.contains("leg ingress stalled")));
    assert!(rows.iter().any(|line| line.contains("leg ingress resumed")));
    for row in rows {
        assert!(
            row.contains("session_id=ingress-full-session-1234567890abcdef"),
            "{row}"
        );
        assert!(row.contains("leg_id=callee"), "{row}");
        assert!(row.contains("direction=\"peer_to_pbx\""), "{row}");
    }
}

// Real leg construction and terminal quality/CDR boundary, without an extra sampler;
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
            diagnostics.len() == 2,
            "prebridge lifecycle must emit start/end only, without periodic logging: {rows}"
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
        assert!(!rows.contains("media.packet_send_tail"));
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
        assert!(report["packetDiagnostics"]["unmeasured_boundaries"].as_array().unwrap()
            .iter().any(|value| value == "socket_send_completion"));
        let found = logs.search("diagnostic-full-session-12ab").await;
        assert_eq!(
            found
                .iter()
                .filter(|row| row.contains("media.packet_stages"))
                .count(),
            diagnostics.len()
        );

        // Core lifecycle evidence is unconditional, even with quality sampling disabled.
        config.call_id = Some("diagnostic-quality-disabled-session".into());
        config.quality_stats.enabled = false;
        let disabled = LegInner::new("disabled-leg", &config, None).unwrap();
        disabled.stop();
        assert_eq!(logs.text().lines().filter(|row| row.contains("media.packet_stages")
            && row.contains("diagnostic-quality-disabled-session")).count(), 2);

    });
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
    let held = serde_json::to_value(b.quality_report("B")).unwrap();
    assert_eq!(
        held["packetDiagnostics"]["pipeline_egress"], 0,
        "a held target must not claim plaintext egress"
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

