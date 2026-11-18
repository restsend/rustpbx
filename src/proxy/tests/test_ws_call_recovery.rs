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

pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(port: u16) -> Ws {
    connect_ws(port, false).await
}

pub(crate) async fn connect_ws(port: u16, secure: bool) -> Ws {
    let mut request = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "sip".parse().unwrap());
    if secure {
        request.headers_mut().insert("x-forwarded-proto", "https".parse().unwrap());
    }
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

// Use the production HTTP-upgraded channel adapter, including its close semantics.
pub(crate) async fn serve_ws(
    ws_listener: tokio::net::TcpListener,
    inner: Arc<crate::proxy::server::SipServerInner>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let endpoint = inner.endpoint.inner.clone();
    let ws_cancel = cancel.clone();
    let ws_router = axum::Router::new().route(
        "/",
        axum::routing::get(
            async move |client: crate::handler::middleware::clientaddr::ClientAddr,
                        upgrade: axum::extract::WebSocketUpgrade| {
                upgrade.protocols(["sip"]).on_upgrade(async move |socket| {
                    crate::proxy::ws::sip_ws_handler(ws_cancel, client, socket, endpoint).await;
                })
            },
        ),
    );
    tokio::spawn(async move {
        axum::serve(
            ws_listener,
            ws_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    })
}

// Model the already-deployed owner entry: tagged SIP requests dispatch
// straight to its existing dialog, with no new reconnect relay involved.
struct ExistingDialogSipOwner(crate::proxy::server::SipServerRef);

impl ExistingDialogSipOwner {
    fn create(
        server: crate::proxy::server::SipServerRef,
        _: Arc<ProxyConfig>,
    ) -> anyhow::Result<Box<dyn crate::proxy::ProxyModule>> {
        Ok(Box::new(Self(server)))
    }
}

#[async_trait::async_trait]
impl crate::proxy::ProxyModule for ExistingDialogSipOwner {
    fn name(&self) -> &str {
        "existing-dialog-owner"
    }
    async fn on_start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_stop(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn on_transaction_begin(
        &self,
        _: CancellationToken,
        tx: &mut rsipstack::transaction::transaction::Transaction,
        _: crate::call::TransactionCookie,
    ) -> anyhow::Result<crate::proxy::ProxyAction> {
        use rsipstack::transaction::key::TransactionRole;
        let id = rsipstack::dialog::DialogId::try_from((&tx.original, TransactionRole::Server))?;
        if tx.original.method == rsipstack::sip::Method::Invite
            && !id.local_tag.is_empty()
            && tx
                .connection
                .as_ref()
                .and_then(|c| c.get_remote_addr())
                .is_some_and(|addr| addr.r#type == Some(rsipstack::sip::Transport::Udp))
        {
            if tx
                .original
                .headers
                .iter()
                .any(|h| h.name().eq_ignore_ascii_case("X-Test-Reject"))
            {
                tx.reply(rsipstack::sip::StatusCode::RequestPending).await?;
                let ack = tokio::time::timeout(Duration::from_secs(3), tx.receive())
                    .await
                    .expect("relay must ACK a rejected INVITE without waiting for the phone");
                assert!(
                    matches!(ack, Some(SipMessage::Request(ref req)) if req.method == rsipstack::sip::Method::Ack)
                );
                return Ok(crate::proxy::ProxyAction::Abort);
            }
            let mut dialog = self
                .0
                .dialog_layer
                .get_dialog(&id)
                .expect("old owner must already hold original dialog");
            if tx.original.cseq_header()?.typed()?.seq == 11 {
                // Lose the first actual UDP ACK before the old transaction
                // consumes it. Its real Timer G must retransmit the 200;
                // the new home must replay the phone's identical cached ACK.
                use rsipstack::transaction::transaction::TransactionEvent;
                use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
                let (filtered_tx, filtered_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut incoming = std::mem::replace(&mut tx.tu_receiver, filtered_rx);
                let acks = Arc::new(AtomicUsize::new(0));
                let identical = Arc::new(AtomicBool::new(true));
                let bridge = tokio::spawn({
                    let acks = acks.clone();
                    let identical = identical.clone();
                    async move {
                        let mut first_ack = None;
                        while let Some(event) = incoming.recv().await {
                            if let TransactionEvent::Received(SipMessage::Request(request), _) =
                                &event
                                && request.method == rsipstack::sip::Method::Ack
                            {
                                acks.fetch_add(1, Ordering::Relaxed);
                                let text = request.to_string();
                                if let Some(first) = &first_ack {
                                    identical.store(first == &text, Ordering::Relaxed);
                                } else {
                                    first_ack = Some(text);
                                    continue;
                                }
                            }
                            if filtered_tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                });
                let result = dialog.handle(tx).await;
                bridge.abort();
                result?;
                assert!(
                    acks.load(Ordering::Relaxed) >= 2,
                    "lost first UDP ACK must be replayed after the real owner retransmits 200"
                );
                assert!(
                    identical.load(Ordering::Relaxed),
                    "replay must retain the phone's exact ACK"
                );
            } else {
                dialog.handle(tx).await?;
            }
            return Ok(crate::proxy::ProxyAction::Abort);
        }
        Ok(crate::proxy::ProxyAction::Continue)
    }
}

pub(crate) async fn receive(ws: &mut Ws) -> SipMessage {
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
    send_as(ws, port, method, to, call_id, cseq, body, "alice").await;
}

async fn send_as(
    ws: &mut Ws,
    port: u16,
    method: &str,
    to: &str,
    call_id: &str,
    cseq: u32,
    body: &str,
    from_user: &str,
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
        "{method} sip:{target} SIP/2.0\r\nVia: SIP/2.0/WS 127.0.0.1:9999;branch=z9hG4bK-{method}-{cseq}\r\nFrom: <sip:{from_user}@127.0.0.1>;tag=recovery-caller\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: {cseq} {method}\r\nContact: <sip:alice@127.0.0.1:9999;transport=ws>\r\nMax-Forwards: 70\r\nExpires: 60\r\n{content_type}Content-Length: {}\r\n\r\n{body}", body.len()
    ).into())).await.unwrap();
}

pub(crate) async fn register(ws: &mut Ws, port: u16, generation: u32) {
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

#[tokio::test]
async fn websocket_held_call_receives_remote_bye_after_reregistration() {
    exercise_outbound_connection_loss(FlowLoss::HeldRemoteBye).await;
}

#[tokio::test]
async fn websocket_held_call_receives_remote_bye_after_reregistration_on_other_node() {
    exercise_outbound_connection_loss(FlowLoss::HeldRemoteByeOtherNode).await;
}

#[tokio::test]
async fn websocket_outbound_caller_id_override_resumes_on_other_node() {
    exercise_outbound_connection_loss(FlowLoss::OutboundCallerIdOtherNode).await;
}

#[derive(Clone, Copy)]
enum FlowLoss {
    Ringing,
    BeforeAck,
    Established,
    MediaLost,
    HeldRemoteBye,
    HeldRemoteByeOtherNode,
    OutboundCallerIdOtherNode,
}

async fn exercise_outbound_connection_loss(loss: FlowLoss) {
    let held_remote_bye = matches!(
        loss,
        FlowLoss::HeldRemoteBye | FlowLoss::HeldRemoteByeOtherNode
    );
    let caller_id_override = matches!(loss, FlowLoss::OutboundCallerIdOtherNode);
    let cross_node = matches!(
        loss,
        FlowLoss::HeldRemoteByeOtherNode | FlowLoss::OutboundCallerIdOtherNode
    );
    let answer_before_loss = !matches!(loss, FlowLoss::Ringing);
    let ack_before_loss = matches!(
        loss,
        FlowLoss::Established
            | FlowLoss::MediaLost
            | FlowLoss::HeldRemoteBye
            | FlowLoss::HeldRemoteByeOtherNode
            | FlowLoss::OutboundCallerIdOtherNode
    );
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rustpbx::proxy=debug")
        .with_test_writer()
        .try_init();
    let port = portpicker::pick_unused_port().unwrap();
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = ws_listener.local_addr().unwrap().port();
    let recordings = tempfile::tempdir().unwrap();
    let registration_db = tempfile::tempdir().unwrap();
    let other_port = portpicker::pick_unused_port().unwrap();
    let customer_port = if cross_node {
        portpicker::pick_unused_port().unwrap()
    } else {
        port
    };
    let peers: Vec<std::net::SocketAddr> = [port, other_port, customer_port]
        .into_iter()
        .map(|port| format!("127.0.0.1:{port}").parse().unwrap())
        .collect();
    let cluster_config = crate::config::ClusterConfig {
        peers: peers
            .iter()
            .map(|peer| crate::config::ClusterPeer {
                addr: peer.ip().to_string(),
                sip_port: peer.port(),
                ami_port: 0,
            })
            .collect(),
        session_registry_backend: "db".into(),
        ..Default::default()
    };
    let config = Arc::new(ProxyConfig {
        locator: if cross_node {
            crate::config::LocatorConfig::Database {
                url: format!(
                    "sqlite://{}?mode=rwc",
                    registration_db
                        .path()
                        .join("registrations.sqlite")
                        .display()
                ),
            }
        } else {
            crate::config::LocatorConfig::Memory
        },
        addr: "127.0.0.1".into(),
        udp_port: Some(port),
        ws_port: None,
        modules: Some(vec![
            "registrar".into(),
            "existing-dialog-owner".into(),
            "call".into(),
        ]),
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
    // Use the real owner directory on both nodes: original dialog aliases
    // must be published by the real session path, never injected by the test.
    use sea_orm_migration::{MigrationTrait, SchemaManager};
    let shared_database = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
    crate::models::cluster_session::Migration
        .up(&SchemaManager::new(&shared_database))
        .await
        .unwrap();
    let data_context = Arc::new(
        crate::proxy::data::ProxyDataContext::new(config.clone(), None)
            .await
            .unwrap(),
    );
    let users = MemoryUserBackend::new(None);
    for user in standard_test_users() {
        users.create_user(user).await.unwrap();
    }
    let cancel = CancellationToken::new();
    let _guard = cancel.clone().drop_guard();
    let (cdrs, sender) = CdrCapture::new();
    let server = Arc::new(
        SipServerBuilder::new(config.clone())
            .with_database_connection(shared_database.clone())
            .with_data_context(data_context.clone())
            .with_cluster_peers(if cross_node { peers.clone() } else { vec![] })
            .with_cluster_config(cross_node.then(|| cluster_config.clone()))
            .with_user_backend(Box::new(users))
            .with_cancel_token(cancel.clone())
            .with_callrecord_sender(Some(sender))
            .register_module("registrar", RegistrarModule::create)
            .register_module("existing-dialog-owner", ExistingDialogSipOwner::create)
            .register_module("call", CallModule::create)
            .build()
            .await
            .unwrap(),
    );
    let inner = server.get_inner();
    let ws_serving = serve_ws(ws_listener, inner.clone(), cancel.clone()).await;
    let mut replacement_ws_port = ws_port;
    let mut other_serving = None;
    let mut other_ws_serving = None;
    let mut other_inner = None;
    let mut customer_serving = None;
    if cross_node {
        let users = MemoryUserBackend::new(None);
        for user in standard_test_users() {
            users.create_user(user).await.unwrap();
        }
        let other = Arc::new(
            SipServerBuilder::new(Arc::new(ProxyConfig {
                udp_port: Some(other_port),
                modules: Some(vec!["registrar".into(), "call".into()]),
                recording: None,
                ..config.as_ref().clone()
            }))
            .with_database_connection(shared_database.clone())
            .with_data_context(data_context.clone())
            .with_cluster_peers(peers.clone())
            .with_cluster_config(Some(cluster_config.clone()))
            .with_user_backend(Box::new(users))
            .with_cancel_token(cancel.child_token())
            .register_module("registrar", RegistrarModule::create)
            .register_module("call", CallModule::create)
            .build()
            .await
            .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        replacement_ws_port = listener.local_addr().unwrap().port();
        other_inner = Some(other.get_inner());
        other_ws_serving = Some(serve_ws(listener, other.get_inner(), cancel.clone()).await);
        other_serving = Some(tokio::spawn(async move { other.serve().await }));
        // The customer always enters a separate call node. Its peer leg
        // reaches the original agent node before that phone changes homes.
        let users = MemoryUserBackend::new(None);
        for user in standard_test_users() {
            users.create_user(user).await.unwrap();
        }
        let customer_node = Arc::new(
            SipServerBuilder::new(Arc::new(ProxyConfig {
                udp_port: Some(customer_port),
                modules: Some(vec!["registrar".into(), "call".into()]),
                recording: None,
                ..config.as_ref().clone()
            }))
            .with_database_connection(shared_database)
            .with_data_context(data_context)
            .with_cluster_peers(peers)
            .with_cluster_config(Some(cluster_config))
            .with_user_backend(Box::new(users))
            .with_cancel_token(cancel.child_token())
            .register_module("registrar", RegistrarModule::create)
            .register_module("call", CallModule::create)
            .build()
            .await
            .unwrap(),
        );
        customer_serving = Some(tokio::spawn(async move { customer_node.serve().await }));
    }
    let serving = tokio::spawn({
        let server = server.clone();
        async move { server.serve().await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let caller_rtp = RtpSender::bind().await.unwrap();
    let callee_rtp = Arc::new(RtpSender::bind().await.unwrap());
    let mut bob = TestUa::new(TestUaConfig {
        username: "bob".into(),
        password: "password456".into(),
        realm: "127.0.0.1".into(),
        local_port: portpicker::pick_unused_port().unwrap(),
        proxy_addr: format!("127.0.0.1:{customer_port}").parse().unwrap(),
        webrtc: false,
        extra_invite_headers: vec![],
    });
    bob.start().await.unwrap();
    bob.register().await.unwrap();
    if held_remote_bye {
        exercise_inbound_held_reconnect(
            bob,
            ws_port,
            replacement_ws_port,
            inner.clone(),
            other_inner.as_ref(),
            &callee_rtp,
            &cdrs,
        )
        .await;
        caller_rtp.stop();
        callee_rtp.stop();
        cancel.cancel();
        serving.abort();
        ws_serving.abort();
        if let Some(task) = other_serving {
            task.abort();
        }
        if let Some(task) = other_ws_serving {
            task.abort();
        }
        if let Some(task) = customer_serving {
            task.abort();
        }
        return;
    }
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
    let from_user = if caller_id_override {
        "18005550100"
    } else {
        "alice"
    };
    send_as(
        &mut ws,
        port,
        "INVITE",
        "<sip:bob@127.0.0.1>",
        call_id,
        1,
        &sdp(caller_rtp.local_addr().unwrap().port()),
        from_user,
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
    send_as(&mut ws, port, "ACK", &to, call_id, 1, "", from_user).await;
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

    if caller_id_override {
        let offer = sdp(caller_rtp.local_addr().unwrap().port());
        let hold = offer.replace("a=sendrecv", "a=sendonly");
        for (cseq, body) in [(10, hold.as_str()), (11, offer.as_str())] {
            if cseq == 11 {
                drop(ws);
                ws = connect(replacement_ws_port).await;
                register(&mut ws, replacement_ws_port, 2).await;
                let mut stranger = connect(replacement_ws_port).await;
                send_as(
                    &mut stranger,
                    port,
                    "INVITE",
                    &to,
                    call_id,
                    cseq,
                    body,
                    from_user,
                )
                .await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(200), receive(&mut stranger))
                        .await
                        .is_err(),
                    "the alternate From must never authorize an unregistered WS"
                );
                drop(stranger);
            }
            send_as(&mut ws, port, "INVITE", &to, call_id, cseq, body, from_user).await;
            loop {
                let SipMessage::Response(answer) =
                    tokio::time::timeout(Duration::from_secs(4), receive(&mut ws))
                        .await
                        .unwrap()
                else {
                    panic!("alternate From re-INVITE response");
                };
                if answer.status_code.code() < 200 {
                    continue;
                }
                assert_eq!(answer.status_code.code(), 200);
                assert_eq!(
                    answer.from_header().unwrap().typed().unwrap().uri.user(),
                    Some(from_user)
                );
                if cseq == 11 {
                    let SipMessage::Response(duplicate) =
                        tokio::time::timeout(Duration::from_secs(2), receive(&mut ws))
                            .await
                            .unwrap()
                    else {
                        panic!("owner retransmitted 200");
                    };
                    assert_eq!(duplicate.status_code.code(), 200);
                }
                break;
            }
            send_as(&mut ws, port, "ACK", &to, call_id, cseq, "", from_user).await;
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while inner
                .endpoint
                .inner
                .waiting_ack
                .with(|entries| entries.keys().any(|id| id.call_id == call_id))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("original alternate-From dialog must consume the replayed ACK");
        assert!(inner.active_call_registry.get(&session_id).is_some());
        inner
            .active_call_registry
            .get_handle(&session_id)
            .unwrap()
            .send_command(crate::call::domain::CallCommand::Hangup(
                crate::call::domain::HangupCommand::local("caller_id_reconnect_test", None, None),
            ))
            .unwrap();
        let bye = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                match receive(&mut ws).await {
                    SipMessage::Request(request) => break request,
                    SipMessage::Response(response) => {
                        assert_eq!(response.status_code.code(), 200);
                        assert_eq!(response.cseq_header().unwrap().typed().unwrap().seq, 11);
                        // A 200 already queued on the replacement flow still needs its ACK.
                        send_as(&mut ws, port, "ACK", &to, call_id, 11, "", from_user).await;
                    }
                }
            }
        }).await.expect("original caller dialog BYE");
        assert_eq!(bye.method, rsipstack::sip::Method::Bye);
        phone_reply(&mut ws, &bye, None, "").await;
        let record = cdrs
            .wait_for_record(&session_id, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(record.recorder.len(), 1);
        assert!(std::fs::metadata(&record.recorder[0].path).unwrap().len() > 44);
        caller_rtp.stop();
        callee_rtp.stop();
        cancel.cancel();
        peer.abort();
        serving.abort();
        ws_serving.abort();
        if let Some(task) = other_serving {
            task.abort();
        }
        if let Some(task) = other_ws_serving {
            task.abort();
        }
        if let Some(task) = customer_serving {
            task.abort();
        }
        return;
    }

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

async fn phone_reply(
    ws: &mut Ws,
    request: &rsipstack::sip::Request,
    tag: Option<&str>,
    body: &str,
) {
    use rsipstack::sip::{Header, Response, StatusCode, Version};
    let mut headers: Vec<_> = request
        .headers
        .iter()
        .filter(|header| {
            matches!(
                header,
                Header::Via(_)
                    | Header::From(_)
                    | Header::To(_)
                    | Header::CallId(_)
                    | Header::CSeq(_)
            )
        })
        .cloned()
        .collect();
    if let Some(tag) = tag {
        headers.retain(|header| !matches!(header, Header::To(_)));
        headers.push(Header::To(
            format!("{};tag={tag}", request.to_header().unwrap().value()).into(),
        ));
    }
    headers.push(Header::Contact(
        "<sip:alice@127.0.0.1:9999;transport=ws>".into(),
    ));
    if !body.is_empty() {
        headers.push(Header::ContentType("application/sdp".into()));
    }
    headers.push(Header::ContentLength(body.len().to_string().into()));
    let response = Response {
        status_code: StatusCode::OK,
        wire_reason: None,
        version: Version::V2,
        headers: headers.into(),
        body: body.as_bytes().to_vec(),
    };
    ws.send(Message::Text(response.to_string().into()))
        .await
        .unwrap();
}

// The incident's direction: a customer calls in over SIP; the PBX creates a
// client dialog to a registered WebSocket phone whose registration later moves.
async fn exercise_inbound_held_reconnect(
    customer: TestUa,
    ws_port: u16,
    replacement_ws_port: u16,
    server: Arc<crate::proxy::server::SipServerInner>,
    replacement: Option<&Arc<crate::proxy::server::SipServerInner>>,
    customer_rtp: &RtpSender,
    cdrs: &CdrCapture,
) {
    let mut ws = connect(ws_port).await;
    register(&mut ws, ws_port, 1).await;
    let customer_offer = sdp(customer_rtp.local_addr().unwrap().port());
    let calling = tokio::spawn(async move {
        let id = customer
            .make_call("alice", Some(customer_offer))
            .await
            .unwrap();
        (customer, id)
    });
    let SipMessage::Request(invite) =
        tokio::time::timeout(Duration::from_secs(5), receive(&mut ws))
            .await
            .unwrap()
    else {
        panic!("registered phone must receive INVITE");
    };
    assert_eq!(invite.method, rsipstack::sip::Method::Invite);
    let wire_call_id = invite.call_id_header().unwrap().value().to_string();
    let mut capabilities = rustrtc::MediaCapabilities::default();
    capabilities.audio = vec![rustrtc::config::AudioCapability::pcmu()];
    let pc = Arc::new(rustrtc::PeerConnection::new(rustrtc::RtcConfiguration {
        bind_ips: vec!["127.0.0.1".parse().unwrap()],
        media_capabilities: Some(capabilities),
        ..rustrtc::RtcConfiguration::webrtc()
    }));
    pc.add_transceiver(
        rustrtc::MediaKind::Audio,
        rustrtc::TransceiverDirection::SendRecv,
    );
    pc.set_remote_description(
        rustrtc::SessionDescription::parse(
            rustrtc::SdpType::Offer,
            std::str::from_utf8(&invite.body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    pc.create_answer().await.unwrap();
    pc.wait_for_gathering_complete().await;
    let mut answer = pc.create_answer().await.unwrap();
    answer.sdp_type = rustrtc::SdpType::Answer;
    pc.set_local_description(answer).unwrap();
    phone_reply(
        &mut ws,
        &invite,
        Some("recovery-agent"),
        &pc.local_description().unwrap().to_sdp_string(),
    )
    .await;
    let SipMessage::Request(ack) = tokio::time::timeout(Duration::from_secs(3), receive(&mut ws))
        .await
        .unwrap()
    else {
        panic!("ACK");
    };
    assert_eq!(ack.method, rsipstack::sip::Method::Ack);
    let (customer, customer_dialog) = calling.await.unwrap();
    customer
        .set_answer_sdp(
            &customer_dialog,
            &sdp(customer_rtp.local_addr().unwrap().port()),
        )
        .await;
    let customer_target = extract_media_endpoint(
        &customer
            .get_negotiated_answer_sdp(&customer_dialog)
            .await
            .unwrap(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), pc.wait_for_connected())
        .await
        .unwrap()
        .unwrap();
    customer_rtp.start_sending(
        customer_target,
        (0..1000)
            .map(|n| RtpPacket::new(0, n, u32::from(n) * 160, 2, vec![0x00; 160]))
            .collect(),
        20,
    );
    let audio_marker = Arc::new(std::sync::atomic::AtomicU8::new(0x00));
    let media = tokio::spawn({
        let audio_marker = audio_marker.clone();
        let pc = pc.clone();
        async move {
            let ssrc = pc
                .get_transceivers()
                .into_iter()
                .find_map(|transceiver| transceiver.sender_ssrc())
                .unwrap();
            for n in 0..1000u16 {
                pc.send_raw_rtp(rustrtc::rtp::RtpPacket::new(
                    rustrtc::rtp::RtpHeader::new(0, n, u32::from(n) * 160, ssrc),
                    vec![audio_marker.load(std::sync::atomic::Ordering::Relaxed); 160],
                ))
                .await
                .unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    });
    customer_rtp
        .receive_latest(Duration::from_secs(3))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while pc.received_rtp_packets() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("WebRTC phone must receive real customer audio");
    let session_id = server
        .active_call_registry
        .session_ids()
        .into_iter()
        .next()
        .unwrap();
    let (hangup_tx, mut hangup_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let peer = tokio::spawn(async move {
        loop {
            if hangup_rx.try_recv().is_ok() {
                customer.hangup(&customer_dialog).await.unwrap();
                return;
            }
            customer.process_dialog_events().await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let local_from = format!("{};tag=recovery-agent", invite.to_header().unwrap().value());
    let remote_to = invite.from_header().unwrap().value();
    let target = invite.contact_header().unwrap().typed().unwrap().uri;
    phone_renegotiate(
        &mut ws,
        &pc,
        &target,
        &local_from,
        remote_to,
        &wire_call_id,
        10,
        rustrtc::TransceiverDirection::SendOnly,
        false,
    )
    .await;
    let contact = "sip:alice@127.0.0.1:9999;transport=ws".parse().unwrap();
    let old = server.locator.lookup(&contact).await.unwrap().remove(0);
    drop(ws);
    ws = connect(replacement_ws_port).await;
    register(&mut ws, replacement_ws_port, 2).await;
    let new = server.locator.lookup(&contact).await.unwrap().remove(0);
    assert_ne!(old.destination, new.destination);
    if let Some(replacement) = replacement {
        assert_eq!(new.home_proxy, replacement.cluster_self_addr);
        assert!(replacement.dialog_layer.all_dialog_ids().is_empty());
    }
    assert!(server.active_call_registry.get(&session_id).is_some());
    assert!(cdrs.get_all_records().await.is_empty());
    if replacement.is_some() {
        let mut unregistered = connect(replacement_ws_port).await;
        let body = pc.local_description().unwrap().to_sdp_string();
        let request = format!(
            "INVITE {target} SIP/2.0\r\nVia: SIP/2.0/WS 127.0.0.1:9999;branch=z9hG4bK-unregistered\r\nFrom: {local_from}\r\nTo: {remote_to}\r\nCall-ID: {wire_call_id}\r\nCSeq: 11 INVITE\r\nContact: <sip:alice@127.0.0.1:9999;transport=ws>\r\nMax-Forwards: 70\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        unregistered
            .send(Message::Text(request.into()))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), receive(&mut unregistered))
                .await
                .is_err(),
            "a different WS flow must never gain access to this registered dialog"
        );
        drop(unregistered);
    }
    phone_renegotiate(
        &mut ws,
        &pc,
        &target,
        &local_from,
        remote_to,
        &wire_call_id,
        11,
        rustrtc::TransceiverDirection::SendRecv,
        replacement.is_some(),
    )
    .await;
    // The real owner must consume this ACK, rather than just replying 200.
    tokio::time::timeout(Duration::from_secs(3), async {
        while server
            .endpoint
            .inner
            .waiting_ack
            .with(|entries| entries.keys().any(|id| id.call_id == wire_call_id))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("original owner must consume resumed dialog ACK");
    let received_before = pc.received_rtp_packets();
    audio_marker.store(0x80, std::sync::atomic::Ordering::Relaxed);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let packet = customer_rtp
                .receive_latest(Duration::from_secs(2))
                .await
                .unwrap();
            if packet.payload.contains(&0x80) && pc.received_rtp_packets() > received_before + 10 {
                break;
            }
        }
    })
    .await
    .expect("bidirectional media must resume on the original ICE/DTLS session");
    if replacement.is_some() {
        let body = pc.local_description().unwrap().to_sdp_string();
        let request = format!(
            "INVITE {target} SIP/2.0\r\nVia: SIP/2.0/WS 127.0.0.1:9999;branch=z9hG4bK-rejected-12\r\nFrom: {local_from}\r\nTo: {remote_to}\r\nCall-ID: {wire_call_id}\r\nCSeq: 12 INVITE\r\nContact: <sip:alice@127.0.0.1:9999;transport=ws>\r\nMax-Forwards: 70\r\nX-Test-Reject: yes\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        ws.send(Message::Text(request.into())).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let SipMessage::Response(response) = receive(&mut ws).await else {
                    panic!("491 response");
                };
                let seq = response.cseq_header().unwrap().typed().unwrap().seq;
                if seq == 12 {
                    break response;
                }
                assert_eq!(seq, 11, "only the preceding accepted INVITE may retransmit");
                assert_eq!(response.status_code.code(), 200);
                ws.send(Message::Text(format!("ACK {target} SIP/2.0\r\nVia: SIP/2.0/WS 127.0.0.1:9999;branch=z9hG4bK-recovery-ack-11\r\nFrom: {local_from}\r\nTo: {remote_to}\r\nCall-ID: {wire_call_id}\r\nCSeq: 11 ACK\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n").into())).await.unwrap();
            }
        }).await.expect("491 response");
        assert_eq!(response.status_code.code(), 491);
        tokio::time::timeout(Duration::from_secs(2), async {
            while server
                .endpoint
                .inner
                .waiting_ack
                .with(|entries| entries.keys().any(|id| id.call_id == wire_call_id))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("non-2xx owner ACK must not wait for the phone's ACK");
        ws.send(Message::Text(format!("ACK {target} SIP/2.0\r\nVia: SIP/2.0/WS 127.0.0.1:9999;branch=z9hG4bK-rejected-12\r\nFrom: {local_from}\r\nTo: {remote_to}\r\nCall-ID: {wire_call_id}\r\nCSeq: 12 ACK\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n").into())).await.unwrap();
    }
    // A second negotiated hold proves the first ACK released the negotiation.
    phone_renegotiate(
        &mut ws,
        &pc,
        &target,
        &local_from,
        remote_to,
        &wire_call_id,
        13,
        rustrtc::TransceiverDirection::SendOnly,
        false,
    )
    .await;
    hangup_tx.send(()).unwrap();
    let SipMessage::Request(bye) = tokio::time::timeout(Duration::from_secs(4), receive(&mut ws))
        .await
        .expect("customer BYE must reach the phone's replacement registered WS flow")
    else {
        panic!("BYE");
    };
    assert_eq!(bye.method, rsipstack::sip::Method::Bye);
    assert_eq!(bye.call_id_header().unwrap().value(), wire_call_id);
    assert_eq!(
        bye.from_header().unwrap().typed().unwrap().tag(),
        invite.from_header().unwrap().typed().unwrap().tag()
    );
    assert_eq!(
        bye.to_header()
            .unwrap()
            .typed()
            .unwrap()
            .tag()
            .unwrap()
            .value(),
        "recovery-agent"
    );
    phone_reply(&mut ws, &bye, None, "").await;
    tokio::time::timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    media.abort();
    pc.close();
    let record = cdrs
        .wait_for_record(&session_id, Duration::from_secs(5))
        .await
        .expect("customer BYE must finish original CDR and recording");
    assert!(record.answer_time.is_some());
    assert_eq!(record.recorder.len(), 1);
    let recording = &record.recorder[0];
    let wav = std::fs::read(&recording.path).unwrap();
    assert!(wav.starts_with(b"RIFF") && wav.len() > 44);
    assert_eq!(recording.size, wav.len() as u64);
    assert_eq!(record.call_id, session_id);
    assert_eq!(
        u16::from_le_bytes(wav[20..22].try_into().unwrap()),
        7,
        "fixture records actual PCMU"
    );
    let mut decoder = audio_codec::create_decoder(audio_codec::CodecType::PCMU);
    let pcm = decoder.decode(&wav[44..]);
    assert!(
        pcm.iter().filter(|sample| **sample < -10000).count() >= 160,
        "recording must retain nonzero pre-disconnect audio"
    );
    assert!(
        pcm.iter().filter(|sample| **sample > 10000).count() >= 160,
        "same recording must retain distinguishable post-resume audio"
    );
    assert!(server.active_call_registry.get(&session_id).is_none());
}

async fn phone_renegotiate(
    ws: &mut Ws,
    pc: &rustrtc::PeerConnection,
    target: &rsipstack::sip::Uri,
    from: &str,
    to: &str,
    call_id: &str,
    cseq: u32,
    direction: rustrtc::TransceiverDirection,
    expect_udp_retransmit: bool,
) {
    for transceiver in pc.get_transceivers() {
        transceiver.set_direction(direction);
    }
    let offer = pc.create_offer().await.unwrap();
    let body = offer.to_sdp_string();
    pc.set_local_description(offer).unwrap();
    let request = format!(
        "INVITE {target} SIP/2.0\r\nVia: SIP/2.0/WS 127.0.0.1:9999;branch=z9hG4bK-recovery-{cseq}\r\nFrom: {from}\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: {cseq} INVITE\r\nContact: <sip:alice@127.0.0.1:9999;transport=ws>\r\nMax-Forwards: 70\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    ws.send(Message::Text(request.into())).await.unwrap();
    loop {
        let SipMessage::Response(response) =
            tokio::time::timeout(Duration::from_secs(4), receive(ws))
                .await
                .expect("re-INVITE must reach the original dialog after registration moves")
        else {
            panic!("re-INVITE response")
        };
        if response.status_code.code() < 200 {
            continue;
        }
        assert_eq!(response.status_code.code(), 200);
        assert_eq!(response.cseq_header().unwrap().typed().unwrap().seq, cseq);
        pc.set_remote_description(
            rustrtc::SessionDescription::parse(
                rustrtc::SdpType::Answer,
                std::str::from_utf8(&response.body).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        if expect_udp_retransmit {
            let SipMessage::Response(duplicate) =
                tokio::time::timeout(Duration::from_secs(2), receive(ws))
                    .await
                    .expect("owner's real retransmitted 200 must survive before the phone ACKs")
            else {
                panic!("duplicate 200")
            };
            assert_eq!(duplicate.status_code.code(), 200);
            assert_eq!(duplicate.cseq_header().unwrap().typed().unwrap().seq, cseq);
            assert_eq!(duplicate.body, response.body);
        }
        break;
    }
    ws.send(Message::Text(format!("ACK {target} SIP/2.0\r\nVia: SIP/2.0/WS 127.0.0.1:9999;branch=z9hG4bK-recovery-ack-{cseq}\r\nFrom: {from}\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: {cseq} ACK\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n").into())).await.unwrap();
}
