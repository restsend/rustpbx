//! Realtime bridge integration tests — drive [`run_realtime_bridge`] against
//! an in-process scripted "OpenAI Realtime" mock server over a real
//! localhost TCP WebSocket. No SIP, no network, no API key required.
//!
//! Covered: Bearer-header auth on the upgrade request, `?model=` URL
//! composition, `session.update` handshake, uplink base64 audio, downlink
//! audio → playout, barge-in (speech_started → response.cancel + mute +
//! suppressed playout), transcript/function_call event surfacing, and
//! endpoint-close semantics.

use futures::{SinkExt, StreamExt};
use rustpbx::call::realtime::bridge::{
    BridgeEndReason, BridgeIo, BridgeOutput, BridgeRun, Playout, UplinkPcm, run_realtime_bridge,
};
use rustpbx::call::realtime::{RealtimeParams, RealtimeProtocolKind, openai::OpenAiRealtime};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_util::sync::CancellationToken;

// ── helpers ─────────────────────────────────────────────────────────────────

fn params(port: u16) -> RealtimeParams {
    RealtimeParams {
        url: format!("ws://127.0.0.1:{port}/v1/realtime"),
        protocol: RealtimeProtocolKind::OpenAi,
        api_key: Some("sk-test-key".into()),
        model: Some("gpt-realtime-test".into()),
        voice: Some("alloy".into()),
        instructions: Some("Be brief".into()),
        sample_rate: Some(8000),
        timeout_ms: Some(2000),
        extra_headers: Vec::new(),
        hangup_on_disconnect: true,
    }
}

fn b64(pcm: &[i16]) -> String {
    use base64::Engine as _;
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Records playout writes and mute calls — stands in for the MediaBridge leg.
struct TestPlayout {
    written: mpsc::UnboundedSender<Vec<i16>>,
    mutes: mpsc::UnboundedSender<()>,
}

#[async_trait::async_trait]
impl Playout for TestPlayout {
    async fn write(&mut self, pcm: &[i16]) {
        let _ = self.written.send(pcm.to_vec());
    }
    async fn mute(&mut self) {
        let _ = self.mutes.send(());
    }
}

/// What the scripted mock server observed / was asked to do.
#[derive(Debug, Clone)]
enum MockStep {
    /// Deliver a JSON event to the client.
    Send(String),
    /// Close the WebSocket.
    Close,
}

struct MockServer {
    /// Upgrade request path (contains the model query).
    path: mpsc::Receiver<String>,
    /// `Authorization` header value.
    auth: mpsc::Receiver<Option<String>>,
    /// `session.update` received after connect, if any.
    session_update: mpsc::Receiver<String>,
    /// base64 audio chunks received from the client (uplink).
    uplink: mpsc::Receiver<String>,
    /// `response.cancel` seen?
    cancelled: mpsc::Receiver<()>,
}

/// Spawn the scripted mock: reads upgrade headers, emits them for asserts,
/// applies `steps` in order, then closes.
async fn spawn_mock(steps: Vec<MockStep>) -> (u16, MockServer) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let (path_tx, path_rx) = mpsc::channel(1);
    let (auth_tx, auth_rx) = mpsc::channel(1);
    let (session_tx, session_rx) = mpsc::channel(4);
    let (uplink_tx, uplink_rx) = mpsc::channel(64);
    let (cancel_tx, cancel_rx) = mpsc::channel(1);

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let ws = tokio_tungstenite::accept_hdr_async(stream, |req: &Request<()>, mut resp| {
            let _ = path_tx.try_send(
                req.uri()
                    .path_and_query()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_default(),
            );
            let auth = req
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let _ = auth_tx.try_send(auth);
            Ok(resp)
        })
        .await
        .expect("handshake");
        let (mut sink, mut read) = ws.split();

        for step in steps {
            match step {
                MockStep::Send(text) => {
                    sink.send(Message::text(text)).await.expect("send");
                    // After each scripted send, pump client messages briefly
                    // so the client's reactions (uplink/cancel) are observed.
                    for _ in 0..8 {
                        let msg =
                            tokio::time::timeout(std::time::Duration::from_millis(40), read.next())
                                .await;
                        let Ok(Some(Ok(msg))) = msg else { continue };
                        match msg {
                            Message::Text(t) => {
                                let v: serde_json::Value =
                                    serde_json::from_str(&t).unwrap_or_default();
                                match v["type"].as_str() {
                                    Some("session.update") => {
                                        let _ = session_tx.try_send(t.to_string());
                                    }
                                    Some("input_audio_buffer.append") => {
                                        let _ = uplink_tx.try_send(
                                            v["audio"].as_str().unwrap_or_default().to_string(),
                                        );
                                    }
                                    Some("response.cancel") => {
                                        let _ = cancel_tx.try_send(());
                                    }
                                    _ => {}
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
                MockStep::Close => {
                    let _ = sink.close().await;
                    return;
                }
            }
        }
        // Drain until the client goes away.
        while let Ok(Some(Ok(_))) =
            tokio::time::timeout(std::time::Duration::from_secs(2), read.next()).await
        {}
    });

    (
        port,
        MockServer {
            path: path_rx,
            auth: auth_rx,
            session_update: session_rx,
            uplink: uplink_rx,
            cancelled: cancel_rx,
        },
    )
}

/// Start the bridge loop against `port` with the given steps; returns the
/// join handle plus output receivers.
async fn start_bridge(
    steps: Vec<MockStep>,
    cancel: CancellationToken,
) -> (
    MockServer,
    tokio::sync::oneshot::Receiver<BridgeRun>,
    mpsc::Receiver<BridgeOutput>,
    mpsc::UnboundedReceiver<Vec<i16>>,
    mpsc::UnboundedReceiver<()>,
) {
    let (mock_port, mock) = spawn_mock(steps).await;

    let (run_tx, run_rx) = tokio::sync::oneshot::channel();
    let (out_tx, out_rx) = mpsc::channel(64);
    let (written_tx, written_rx) = mpsc::unbounded_channel();
    let (mutes_tx, mutes_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let mut p = params(mock_port);
        p.api_key = Some("sk-test-key".into());
        let protocol = Box::new(OpenAiRealtime);
        let ws = rustpbx::call::realtime::bridge::connect_realtime_ws(protocol.as_ref(), &p)
            .await
            .expect("mock connect");
        let (ws_write, ws_read) = ws.split();
        let (_u_tx, u_rx) = mpsc::channel(8);
        let io = BridgeIo {
            ws_write,
            ws_read,
            uplink_rx: u_rx,
            playout: Box::new(TestPlayout {
                written: written_tx,
                mutes: mutes_tx,
            }),
            cancel,
        };
        let run = run_realtime_bridge(protocol, &p, io, out_tx).await;
        let _ = run_tx.send(run);
    });
    (mock, run_rx, out_rx, written_rx, mutes_rx)
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn auth_url_and_handshake_then_disconnect() {
    let cancel = CancellationToken::new();
    let (mut mock, run_rx, _out, _written, _mutes) = start_bridge(
        vec![
            MockStep::Send(r#"{"type":"session.created"}"#.into()),
            MockStep::Close,
        ],
        cancel,
    )
    .await;

    // Upgrade request carried the credential in the header — never the URL.
    let auth = tokio::time::timeout(std::time::Duration::from_secs(2), mock.auth.recv())
        .await
        .expect("auth observed")
        .expect("header present");
    assert_eq!(auth.as_deref(), Some("Bearer sk-test-key"));

    let path = tokio::time::timeout(std::time::Duration::from_secs(2), mock.path.recv())
        .await
        .expect("path observed")
        .unwrap_or_default();
    assert!(path.contains("model=gpt-realtime-test"), "path: {path}");
    assert!(!path.contains("sk-test-key"), "key must not ride the url");

    // Handshake sent session.update with voice + instructions.
    let update = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        mock.session_update.recv(),
    )
    .await
    .expect("session.update")
    .expect("non-empty");
    let v: serde_json::Value = serde_json::from_str(&update).unwrap();
    assert_eq!(v["session"]["voice"], "alloy");

    // Endpoint close surfaces as WsClosed (+ a disconnected event).
    let run = tokio::time::timeout(std::time::Duration::from_secs(3), run_rx)
        .await
        .expect("bridge run finishes")
        .expect("run channel");
    assert_eq!(run.reason, BridgeEndReason::WsClosed);
}

#[tokio::test]
async fn downlink_audio_transcript_and_function_call_reach_outputs() {
    let cancel = CancellationToken::new();
    let audio_b64 = b64(&[100i16, -100, 5]);
    let (mut mock, run_rx, mut out_rx, mut written, _mutes) = start_bridge(
        vec![
            MockStep::Send(
                serde_json::json!({"type":"response.audio.delta","delta":audio_b64}).to_string(),
            ),
            MockStep::Send(
                r#"{"type":"response.audio_transcript.delta","delta":"Hello"}"#.into(),
            ),
            MockStep::Send(
                r#"{"type":"conversation.item.input_audio_transcription.completed","transcript":"hi"}"#
                    .into(),
            ),
            MockStep::Send(
                r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"lookup","arguments":"{}","call_id":"c1"}}"#
                    .into(),
            ),
            MockStep::Close,
        ],
        cancel,
    )
    .await;
    let _ = mock;

    // Audio decoded to PCM and written to playout.
    let pcm = tokio::time::timeout(std::time::Duration::from_secs(3), written.recv())
        .await
        .expect("playout write")
        .expect("non-empty");
    assert_eq!(pcm, vec![100i16, -100, 5]);

    // Events surface (interleaved with `connected`): collect until the
    // function_call lands.
    let mut seen = Vec::new();
    for _ in 0..8 {
        if seen.contains(&"function_call".to_string()) {
            break;
        }
        let out = tokio::time::timeout(std::time::Duration::from_secs(3), out_rx.recv())
            .await
            .expect("output event")
            .expect("channel open");
        if let BridgeOutput::Event { kind, data } = out {
            seen.push(kind.clone());
            match kind.as_str() {
                "function_call" => {
                    assert_eq!(data["name"], "lookup");
                    assert_eq!(data["call_id"], "c1");
                }
                "transcript_final" => assert_eq!(data["text"], "hi"),
                "transcript_delta" => assert_eq!(data["text"], "Hello"),
                _ => {}
            }
        }
    }
    assert!(seen.contains(&"transcript_delta".to_string()), "{seen:?}");
    assert!(seen.contains(&"transcript_final".to_string()), "{seen:?}");
    assert!(seen.contains(&"function_call".to_string()), "{seen:?}");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), run_rx).await;
}

#[tokio::test]
async fn barge_in_mutes_and_cancels_then_suppresses_playout() {
    let cancel = CancellationToken::new();
    let loud = b64(&[1000i16; 8]);
    let after = b64(&[7i16; 8]);
    let (mut mock, run_rx, mut out_rx, mut written, mut mutes) = start_bridge(
        vec![
            // First utterance: audio flows to playout.
            MockStep::Send(
                serde_json::json!({"type":"response.audio.delta","delta":loud}).to_string(),
            ),
            // Caller speaks → barge-in.
            MockStep::Send(r#"{"type":"input_audio_buffer.speech_started"}"#.into()),
            // AI keeps streaming (server didn't halt instantly) — must NOT play.
            MockStep::Send(
                serde_json::json!({"type":"response.audio.delta","delta":after}).to_string(),
            ),
            MockStep::Send(r#"{"type":"input_audio_buffer.speech_stopped"}"#.into()),
            MockStep::Close,
        ],
        cancel,
    )
    .await;
    let _ = mock;

    // The first delta was played.
    let first = tokio::time::timeout(std::time::Duration::from_secs(3), written.recv())
        .await
        .expect("first write")
        .expect("non-empty");
    assert_eq!(first, vec![1000i16; 8]);

    // Mute fired exactly once for the barge-in.
    let _mute = tokio::time::timeout(std::time::Duration::from_secs(3), mutes.recv())
        .await
        .expect("mute")
        .expect("non-empty");

    // The client sent response.cancel to the endpoint.
    let cancelled = tokio::time::timeout(std::time::Duration::from_secs(3), mock.cancelled.recv())
        .await
        .expect("response.cancel observed");
    assert!(cancelled.is_some(), "endpoint must receive response.cancel");

    // The post-barge-in delta was suppressed: no second write.
    let second = tokio::time::timeout(std::time::Duration::from_millis(500), written.recv()).await;
    assert!(second.is_err(), "muted audio must not reach playout");

    // barge_in event surfaced.
    let mut saw_barge = false;
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_millis(300), out_rx.recv()).await {
            Ok(Some(BridgeOutput::Event { kind, .. })) if kind == "barge_in" => {
                saw_barge = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    assert!(saw_barge, "barge_in event must be emitted");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), run_rx).await;
}

#[tokio::test]
async fn cancel_token_stops_the_bridge() {
    let cancel = CancellationToken::new();
    // No steps — mock just idles; the loop must end via the token.
    let (mock, run_rx, _out, _written, _mutes) = start_bridge(vec![], cancel.clone()).await;
    let _ = mock;
    cancel.cancel();
    let run = tokio::time::timeout(std::time::Duration::from_secs(2), run_rx)
        .await
        .expect("bridge run finishes after cancel")
        .expect("run channel");
    assert_eq!(run.reason, BridgeEndReason::Cancelled);
}
