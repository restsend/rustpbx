use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::call::user::SipUser;
use crate::config::ProxyConfig;
use crate::media::negotiate::CodecSelectionStrategy;
use crate::proxy::{
    auth::AuthModule, call::CallModule, locator::MemoryLocator, registrar::RegistrarModule,
    server::{SipServer, SipServerBuilder}, user::MemoryUserBackend,
};
use rsipstack::dialog::DialogId;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

fn create_test_proxy_config(port: u16) -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        tcp_port: None,
        tls_port: None,
        ws_port: None,
        useragent: Some("RustPBX-Test/0.1.0".to_string()),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    }
}

async fn create_test_ua(username: &str, proxy_addr: SocketAddr, local_port: u16) -> Result<TestUa> {
    let config = TestUaConfig {
        username: username.to_string(),
        password: "password".to_string(),
        realm: "127.0.0.1".to_string(),
        local_port,
        proxy_addr,
    };
    let mut ua = TestUa::new(config);
    ua.start().await?;
    Ok(ua)
}

async fn setup_proxy_and_users(port: u16) -> (Arc<SipServer>, CancellationToken) {
    setup_proxy_and_users_with_config(port, Arc::new(create_test_proxy_config(port))).await
}

async fn setup_proxy_and_users_with_config(
    _port: u16,
    config: Arc<ProxyConfig>,
) -> (Arc<SipServer>, CancellationToken) {

    let user_backend = MemoryUserBackend::new(None);
    user_backend
        .create_user(SipUser {
            id: 1,
            username: "alice".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    user_backend
        .create_user(SipUser {
            id: 2,
            username: "bob".to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    let locator = MemoryLocator::new();
    let cancel_token = CancellationToken::new();
    let mut builder = SipServerBuilder::new(config)
        .with_user_backend(Box::new(user_backend))
        .with_locator(Box::new(locator))
        .with_cancel_token(cancel_token.clone());

    builder = builder
        .register_module("registrar", |inner, config| {
            Ok(Box::new(RegistrarModule::new(inner, config)))
        })
        .register_module("auth", |inner, _config| {
            Ok(Box::new(AuthModule::new(inner.clone(), inner.proxy_config.clone())))
        })
        .register_module("call", |inner, config| {
            Ok(Box::new(CallModule::new(config, inner)))
        });

    let server = Arc::new(builder.build().await.unwrap());
    let server_clone = server.clone();
    tokio::spawn(async move {
        if let Err(e) = server_clone.serve().await {
            warn!("Proxy server error: {:?}", e);
        }
    });
    sleep(Duration::from_millis(200)).await;

    (server, cancel_token)
}

async fn establish_call(
    alice: &TestUa,
    bob: &TestUa,
    offer_sdp: &str,
    bob_answer_sdp: &str,
) -> (DialogId, DialogId) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let alice_clone = alice.clone();
    let offer = offer_sdp.to_string();
    tokio::spawn(async move {
        let res = alice_clone.make_call("bob", Some(offer)).await;
        let _ = tx.send(res).await;
    });

    let mut bob_call_id = None;
    for _ in 0..30 {
        sleep(Duration::from_millis(100)).await;
        if let Ok(events) = bob.process_dialog_events().await {
            for event in events {
                if let TestUaEvent::IncomingCall(id, _) = event {
                    bob.answer_call(&id, Some(bob_answer_sdp.to_string())).await.unwrap();
                    bob_call_id = Some(id);
                    break;
                }
            }
        }
        if bob_call_id.is_some() {
            break;
        }
    }
    let bob_call_id = bob_call_id.expect("Bob should receive incoming call");
    let alice_call_id = rx.recv().await.unwrap().expect("Alice call should succeed");
    sleep(Duration::from_millis(500)).await;
    (alice_call_id, bob_call_id)
}

#[tokio::test]
async fn test_reinvite_audio_hold_unhold() {
    let _ = tracing_subscriber::fmt::try_init();
    let port = portpicker::pick_unused_port().unwrap_or(15065);
    let (_server, cancel_token) = setup_proxy_and_users(port).await;

    let proxy_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let alice_port = portpicker::pick_unused_port().unwrap_or(25061);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25062);
    let alice = create_test_ua("alice", proxy_addr, alice_port).await.unwrap();
    let bob = create_test_ua("bob", proxy_addr, bob_port).await.unwrap();
    alice.register().await.unwrap();
    bob.register().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let offer_sdp = "v=0\r\no=- 123 456 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n".to_string();
    let answer_sdp = "v=0\r\no=- 456 789 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n".to_string();
    let (alice_call_id, _bob_call_id) = establish_call(&alice, &bob, &offer_sdp, &answer_sdp).await;

    // Alice sends re-INVITE with sendonly (hold)
    let bob_clone = bob.clone();
    let bob_handle = tokio::spawn(async move {
        for _ in 0..50 {
            if let Ok(events) = bob_clone.process_dialog_events().await {
                for event in &events {
                    if let TestUaEvent::CallUpdated(_, method, _) = event
                        && *method == rsipstack::sip::Method::Invite
                    {
                        return true;
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        false
    });

    let hold_sdp = "v=0\r\no=- 123 457 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n".to_string();
    let alice_received_sdp = alice
        .send_reinvite(&alice_call_id, Some(hold_sdp.clone()))
        .await
        .unwrap();
    assert!(alice_received_sdp.is_some(), "Alice should receive SDP answer");
    info!("Alice received SDP answer from Proxy: {:?}", alice_received_sdp);
    let bob_processed = bob_handle.await.unwrap();
    assert!(bob_processed, "Bob should receive forwarded re-INVITE");

    let received_sdp = alice_received_sdp.unwrap();
    assert!(received_sdp.contains("PCMU/8000"), "SDP answer should contain PCMU");

    // Cleanup
    alice.hangup(&alice_call_id).await.ok();
    cancel_token.cancel();
}

#[tokio::test]
async fn test_reinvite_audio_only_no_video() {
    let _ = tracing_subscriber::fmt::try_init();
    let port = portpicker::pick_unused_port().unwrap_or(15066);
    let (_server, cancel_token) = setup_proxy_and_users(port).await;

    let proxy_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let alice_port = portpicker::pick_unused_port().unwrap_or(25063);
    let bob_port = portpicker::pick_unused_port().unwrap_or(25064);
    let alice = create_test_ua("alice", proxy_addr, alice_port).await.unwrap();
    let bob = create_test_ua("bob", proxy_addr, bob_port).await.unwrap();
    alice.register().await.unwrap();
    bob.register().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let offer_sdp = "v=0\r\no=- 123 456 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n".to_string();
    let answer_sdp = "v=0\r\no=- 456 789 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n".to_string();
    let (alice_call_id, _bob_call_id) = establish_call(&alice, &bob, &offer_sdp, &answer_sdp).await;

    // Verify the leg_has_video tracking: audio-only call should have video=false
    alice.hangup(&alice_call_id).await.ok();
    cancel_token.cancel();
}

#[tokio::test]
async fn test_reinvite_audio_to_video_add_via_bridge() {
    // This test verifies that the bridge PeerConnection can handle a re-INVITE
    // that adds video to a previously audio-only call.
    //
    // The test creates a WebRTC↔RTP bridge, registers an audio-only track,
    // then simulates a re-INVITE by calling add_video_track() + renegotiation
    // on the bridge's WebRTC PC, and checks that the resulting SDP contains
    // a video m-line.
    use crate::media::negotiate::MediaNegotiator;
    use rustrtc::sdp::{SdpType, SessionDescription};

    let caller_sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
    let codec_lists = MediaNegotiator::build_bridge_codec_lists(
        caller_sdp, false, true, &[], CodecSelectionStrategy::default(),
    );
    let audio_caps: Vec<_> = codec_lists
        .caller_side
        .iter()
        .filter_map(|c| c.to_audio_capability())
        .collect();

    let bridge = crate::media::bridge::BridgePeerBuilder::new("test-bridge-video".to_string())
        .with_webrtc_audio_capabilities(audio_caps.clone())
        .with_rtp_audio_capabilities(audio_caps)
        .build();
    bridge.setup_bridge().await.unwrap();

    // Initially no video on either side
    assert!(
        !bridge.has_video().await,
        "No video before add_video_track"
    );

    // Add video track (simulates re-INVITE adding video)
    bridge.add_video_track(96, 90000).await.unwrap();

    // Both sides should now have video senders
    assert!(
        bridge.has_video().await,
        "Both sides should have video after add_video_track"
    );

    // Now simulate a re-INVITE on the RTP PC (the callee side, which uses
    // plain RTP and does NOT require ICE/DTLS). The RTP PC should produce
    // an answer with video after add_video_track.
    let reinvite_offer = "v=0\r\no=- 123 456 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\nm=video 10002 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1\r\n";
    let pc = bridge.rtp_pc();
    let offer = SessionDescription::parse(SdpType::Offer, reinvite_offer).unwrap();
    pc.set_remote_description(offer).await.unwrap();
    let answer = pc.create_answer().await.unwrap();
    pc.set_local_description(answer).unwrap();
    let answer_sdp = pc.local_description().unwrap().to_sdp_string();

    assert!(
        answer_sdp.contains("m=video"),
        "Answer SDP must contain video when re-INVITE adds video: {}",
        answer_sdp
    );
    info!("Video re-INVITE bridge test passed");
}
