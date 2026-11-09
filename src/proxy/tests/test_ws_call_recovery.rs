//! Real WS loss/re-registration must preserve an established call and its recording.

use super::cdr_capture::CdrCapture;
use super::rtp_utils::{RtpPacket, RtpSender, extract_media_endpoint};
use super::test_helpers::standard_test_users;
use super::test_ua::{TestUa, TestUaConfig, TestUaEvent};
use crate::config::{MediaProxyMode, ProxyConfig, RecordingAutoStartAt, RecordingPolicy};
use crate::proxy::{
    call::CallModule, registrar::RegistrarModule, server::SipServerBuilder, user::MemoryUserBackend,
};
use futures::{SinkExt, StreamExt};
use rsipstack::sip::{SipMessage, prelude::*};
use std::{sync::Arc, time::Duration};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(port: u16) -> Ws {
    let mut request = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "sip".parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn receive(ws: &mut Ws) -> SipMessage {
    loop {
        match ws.next().await.expect("WS remains open").unwrap() {
            Message::Text(text) => return SipMessage::try_from(text.as_bytes()).unwrap(),
            Message::Ping(_) => ws.flush().await.unwrap(),
            message => panic!("unexpected WebSocket message: {message:?}"),
        }
    }
}

async fn send(
    ws: &mut Ws,
    port: u16,
    method: &str,
    to: &str,
    call_id: &str,
    cseq: u32,
    body: &str,
) {
    let target = if method == "REGISTER" {
        format!("127.0.0.1:{port}")
    } else {
        format!("bob@127.0.0.1:{port}")
    };
    let content_type = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/sdp\r\n"
    };
    ws.send(Message::Text(format!(
        "{method} sip:{target} SIP/2.0\r\nVia: SIP/2.0/WS 127.0.0.1:9999;branch=z9hG4bK-{method}-{cseq}\r\nFrom: <sip:alice@127.0.0.1>;tag=recovery-caller\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: {cseq} {method}\r\nContact: <sip:alice@127.0.0.1:9999;transport=ws>\r\nMax-Forwards: 70\r\nExpires: 60\r\n{content_type}Content-Length: {}\r\n\r\n{body}", body.len()
    ).into())).await.unwrap();
}

async fn register(ws: &mut Ws, port: u16, generation: u32) {
    send(
        ws,
        port,
        "REGISTER",
        "<sip:alice@127.0.0.1>",
        "ws-recovery-registration",
        generation,
        "",
    )
    .await;
    let SipMessage::Response(response) = tokio::time::timeout(Duration::from_secs(2), receive(ws))
        .await
        .unwrap()
    else {
        panic!("REGISTER must receive a SIP response");
    };
    assert_eq!(response.status_code.code(), 200);
}

fn sdp(port: u16) -> String {
    format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {port} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n"
    )
}

#[tokio::test]
async fn websocket_outbound_call_and_recording_survive_repeated_connection_loss() {
    exercise_outbound_connection_loss(FlowLoss::Established).await;
}

#[tokio::test]
async fn websocket_outbound_ringing_is_cancelled_on_connection_loss() {
    exercise_outbound_connection_loss(FlowLoss::Ringing).await;
}

#[tokio::test]
async fn websocket_outbound_answer_without_ack_is_cancelled_on_connection_loss() {
    exercise_outbound_connection_loss(FlowLoss::BeforeAck).await;
}

#[tokio::test]
async fn websocket_outbound_unrecoverable_media_loss_finishes_recording_and_cdr() {
    exercise_outbound_connection_loss(FlowLoss::MediaLost).await;
}

#[derive(Clone, Copy)]
enum FlowLoss {
    Ringing,
    BeforeAck,
    Established,
    MediaLost,
}

async fn exercise_outbound_connection_loss(loss: FlowLoss) {
    let answer_before_loss = !matches!(loss, FlowLoss::Ringing);
    let ack_before_loss = matches!(loss, FlowLoss::Established | FlowLoss::MediaLost);
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rustpbx::proxy=debug")
        .with_test_writer()
        .try_init();
    let port = portpicker::pick_unused_port().unwrap();
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = ws_listener.local_addr().unwrap().port();
    let recordings = tempfile::tempdir().unwrap();
    let config = Arc::new(ProxyConfig {
        addr: "127.0.0.1".into(),
        udp_port: Some(port),
        ws_port: None,
        modules: Some(vec!["registrar".into(), "call".into()]),
        ensure_user: Some(false),
        media_proxy: MediaProxyMode::All,
        rtp_timeout: Some(if matches!(loss, FlowLoss::MediaLost) {
            2
        } else {
            60
        }),
        recording: Some(RecordingPolicy {
            enabled: Some(true),
            auto_start: Some(true),
            auto_start_at: Some(RecordingAutoStartAt::Answer),
            path: Some(recordings.path().to_string_lossy().into_owned()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let users = MemoryUserBackend::new(None);
    for user in standard_test_users() {
        users.create_user(user).await.unwrap();
    }
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    let (cdrs, sender) = CdrCapture::new();
    let server = Arc::new(
        SipServerBuilder::new(config)
            .with_user_backend(Box::new(users))
            .with_cancel_token(cancel.clone())
            .with_callrecord_sender(Some(sender))
            .register_module("registrar", RegistrarModule::create)
            .register_module("call", CallModule::create)
            .build()
            .await
            .unwrap(),
    );
    let inner = server.get_inner();
    // Exercise the same HTTP-upgraded channel adapter used by production,
    // not rsipstack's separate native WS listener (which has different close semantics).
    let endpoint = inner.endpoint.inner.clone();
    let ws_cancel = cancel.clone();
    let ws_router = axum::Router::new().route(
        "/",
        axum::routing::get(
            async move |client: crate::handler::middleware::clientaddr::ClientAddr,
                        upgrade: axum::extract::WebSocketUpgrade| {
                upgrade.protocols(["sip"]).on_upgrade(async move |socket| {
                    crate::proxy::ws::sip_ws_handler(ws_cancel, client, socket, endpoint, None).await;
                })
            },
        ),
    );
    let ws_serving = tokio::spawn(async move {
        axum::serve(
            ws_listener,
            ws_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let serving = tokio::spawn({
        let server = server.clone();
        async move { server.serve().await }
    });
    // Listeners are already bound; the REGISTER response below synchronizes startup.

    let caller_rtp = RtpSender::bind().await.unwrap();
    let callee_rtp = Arc::new(RtpSender::bind().await.unwrap());
    let mut bob = TestUa::new(TestUaConfig {
        username: "bob".into(),
        password: "password456".into(),
        realm: "127.0.0.1".into(),
        local_port: portpicker::pick_unused_port().unwrap(),
        proxy_addr: format!("127.0.0.1:{port}").parse().unwrap(),
        webrtc: false,
        extra_invite_headers: vec![],
    });
    bob.start().await.unwrap();
    bob.register().await.unwrap();
    let peer = tokio::spawn({
        let callee_rtp = callee_rtp.clone();
        async move {
            loop {
                for event in bob.process_dialog_events().await.unwrap() {
                    match event {
                        TestUaEvent::IncomingCall(id, offer) => {
                            if !answer_before_loss {
                                bob.send_ringing(&id, None).await.unwrap();
                                continue;
                            }
                            let target = extract_media_endpoint(&offer.unwrap()).unwrap();
                            bob.answer_call(
                                &id,
                                Some(sdp(callee_rtp.local_addr().unwrap().port())),
                            )
                            .await
                            .unwrap();
                            callee_rtp.start_sending(
                                target,
                                (0..1000)
                                    .map(|n| {
                                        RtpPacket::new(0, n, u32::from(n) * 160, 2, vec![0x7f; 160])
                                    })
                                    .collect(),
                                20,
                            );
                        }
                        TestUaEvent::CallTerminated(_) => return,
                        _ => {}
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });
    let mut ws = connect(ws_port).await;
    register(&mut ws, ws_port, 1).await;
    let call_id = "ws-outbound-recovery";
    send(
        &mut ws,
        port,
        "INVITE",
        "<sip:bob@127.0.0.1>",
        call_id,
        1,
        &sdp(caller_rtp.local_addr().unwrap().port()),
    )
    .await;
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let SipMessage::Response(response) = receive(&mut ws).await else {
                panic!("expected INVITE response")
            };
            if response.status_code.code() < 200 {
                if !answer_before_loss && response.status_code.code() == 180 {
                    break response;
                }
                continue;
            }
            assert_eq!(
                response.status_code.code(),
                200,
                "INVITE failed: {response}"
            );
            break response;
        }
    })
    .await
    .unwrap();
    let session_id = inner
        .active_call_registry
        .session_ids()
        .into_iter()
        .next()
        .expect("call must be registered");
    if !answer_before_loss {
        drop(ws);
        tokio::time::timeout(Duration::from_secs(3), peer)
            .await
            .expect("WS loss while ringing must cancel the downstream INVITE")
            .unwrap();
        let record = cdrs
            .wait_for_record(&session_id, Duration::from_secs(5))
            .await
            .expect("cancelled setup must finish its CDR");
        assert!(
            record.answer_time.is_none(),
            "offline caller must not be marked answered"
        );
        assert!(
            record.recorder.is_empty(),
            "unanswered call must not start recording"
        );
        assert!(inner.active_call_registry.get(&session_id).is_none());
        assert_eq!(std::fs::read_dir(recordings.path()).unwrap().count(), 0);
        cancel.cancel();
        serving.abort();
        ws_serving.abort();
        return;
    }
    if !ack_before_loss {
        drop(ws);
        tokio::time::timeout(Duration::from_secs(5), peer)
            .await
            .expect("WS loss before caller ACK must terminate the answered downstream leg")
            .unwrap();
        cdrs.wait_for_record(&session_id, Duration::from_secs(5))
            .await
            .expect("unconfirmed dialog must not leave a live call");
        assert!(inner.active_call_registry.get(&session_id).is_none());
        caller_rtp.stop();
        callee_rtp.stop();
        cancel.cancel();
        serving.abort();
        ws_serving.abort();
        return;
    }
    let to = response.to_header().unwrap().value().to_string();
    send(&mut ws, port, "ACK", &to, call_id, 1, "").await;
    let target = extract_media_endpoint(std::str::from_utf8(&response.body).unwrap()).unwrap();
    caller_rtp.start_sending(
        target,
        (0..1000)
            .map(|n| RtpPacket::new(0, n, u32::from(n) * 160, 1, vec![0x7f; 160]))
            .collect(),
        20,
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if inner
                .dialog_layer
                .all_dialog_ids()
                .iter()
                .filter_map(|id| inner.dialog_layer.get_dialog_with(id))
                .any(|dialog| dialog.id().call_id == call_id && dialog.state().is_confirmed())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("caller ACK must establish the original dialog before the reset");

    if matches!(loss, FlowLoss::MediaLost) {
        caller_rtp
            .receive_latest(Duration::from_secs(2))
            .await
            .unwrap();
        callee_rtp
            .receive_latest(Duration::from_secs(2))
            .await
            .unwrap();
        drop(ws);
        caller_rtp.stop();
        callee_rtp.stop();
        let record = cdrs
            .wait_for_record(&session_id, Duration::from_secs(10))
            .await
            .expect("lost signaling and media must not leave a live call or recording");
        assert_eq!(
            record.hangup_reason,
            Some(crate::callrecord::CallRecordHangupReason::RtpTimeout)
        );
        assert!(record.answer_time.is_some());
        assert_eq!(record.recorder.len(), 1);
        let recording = &record.recorder[0];
        assert!(recording.size > 44);
        assert_eq!(
            std::fs::metadata(&recording.path).unwrap().len(),
            recording.size
        );
        assert!(inner.active_call_registry.get(&session_id).is_none());
        tokio::time::timeout(Duration::from_secs(2), peer)
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
        serving.abort();
        ws_serving.abort();
        return;
    }

    for generation in 2..=3 {
        let caller_before = caller_rtp
            .receive_latest(Duration::from_secs(2))
            .await
            .unwrap();
        let callee_before = callee_rtp
            .receive_latest(Duration::from_secs(2))
            .await
            .unwrap();
        drop(ws); // TCP loss without a WebSocket Close handshake, as during ingress reload.
        ws = connect(ws_port).await;
        register(&mut ws, ws_port, generation).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(3500), receive(&mut ws))
                .await
                .is_err(),
            "reconnected phone must not receive a teardown BYE"
        );
        let caller_after = caller_rtp
            .receive_latest(Duration::from_secs(2))
            .await
            .unwrap();
        let callee_after = callee_rtp
            .receive_latest(Duration::from_secs(2))
            .await
            .unwrap();
        for (before, after) in [(caller_before, caller_after), (callee_before, callee_after)] {
            assert_eq!(after.payload_type, 0);
            assert!(!after.payload.is_empty());
            assert!(
                after.sequence_number.wrapping_sub(before.sequence_number) >= 100,
                "both endpoints must receive fresh RTP throughout the reconnect window"
            );
        }
        assert!(
            inner.active_call_registry.get(&session_id).is_some(),
            "original call must remain active"
        );
        assert!(
            cdrs.get_all_records().await.is_empty(),
            "reconnect must not finish the CDR"
        );
    }

    send(&mut ws, port, "BYE", &to, call_id, 2, "").await;
    let SipMessage::Response(response) =
        tokio::time::timeout(Duration::from_secs(3), receive(&mut ws))
            .await
            .unwrap()
    else {
        panic!("BYE response")
    };
    assert_eq!(response.status_code.code(), 200);
    tokio::time::timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    let record = cdrs
        .wait_for_record(&session_id, Duration::from_secs(5))
        .await
        .expect("explicit BYE must finish the original CDR");
    assert!(record.answer_time.is_some());
    assert_eq!(
        record.recorder.len(),
        1,
        "reconnect must preserve a single recording"
    );
    let recording = &record.recorder[0];
    let wav = std::fs::read(&recording.path).expect("recorded WAV must exist");
    assert!(
        wav.starts_with(b"RIFF") && wav.len() > 44,
        "recording must contain audio"
    );
    let bytes_per_second = u32::from_le_bytes(wav[28..32].try_into().unwrap()) as usize;
    assert!(
        bytes_per_second > 0 && wav.len() - 44 >= bytes_per_second * 6,
        "recording must retain audio across both reconnect windows"
    );
    assert_eq!(recording.size, wav.len() as u64);
    assert!(inner.active_call_registry.get(&session_id).is_none());
    caller_rtp.stop();
    callee_rtp.stop();
    cancel.cancel();
    serving.abort();
    ws_serving.abort();
}
