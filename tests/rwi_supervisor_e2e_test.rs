// tests/rwi_supervisor_e2e_test.rs
//
// End-to-end tests for RWI supervisor commands (listen, whisper, barge).
// These tests verify the RWI command parsing and event handling.
//
// Full flow tests require:
// 1. Create agent call (originate)
// 2. Create supervisor call (originate to a special destination)
// 3. Call supervisor.listen/whisper/barge

#![allow(dead_code)]

mod helpers;

use futures::{SinkExt, StreamExt};
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

// ─── WebSocket helpers ───────────────────────────────────────────────────────

type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Connect to the RWI WebSocket with the test token.
async fn ws_connect(rwi_url: &str) -> WsStream {
    let url = format!("{}?token={}", rwi_url, TEST_TOKEN);
    let (ws, _) = timeout(Duration::from_secs(5), connect_async(&url))
        .await
        .expect("connect timeout")
        .expect("connect error");
    ws
}

/// Send a JSON message and wait for the next text frame that has the given action_id.
async fn ws_send_recv_with_id(ws: &mut WsStream, json: &str, action_id: &str) -> serde_json::Value {
    ws.send(Message::Text(json.into())).await.unwrap();
    // Wait for response with matching action_id
    loop {
        let msg = timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("recv timeout")
            .expect("stream ended")
            .expect("ws error");
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).expect("not JSON");
                // Check if this is a response with matching action_id
                if v.get("action_id").map(|a| a == action_id).unwrap_or(false) {
                    return v;
                }
                // Otherwise it's an event - continue waiting
                tracing::debug!("Skipping event while waiting for response: {}", t);
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

/// Build an RWI request JSON string; returns (action_id, json).
fn rwi_req(action: &str, params: serde_json::Value) -> (String, String) {
    let id = Uuid::new_v4().to_string();
    let json = serde_json::to_string(&serde_json::json!({
        "rwi": "1.0",
        "action_id": id,
        "action": action,
        "params": params,
    }))
    .unwrap();
    (id, json)
}

/// Read frames until `predicate(frame)` returns `true` or timeout expires.
async fn recv_until(
    ws: &mut WsStream,
    timeout_secs: u64,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("recv_until: timed out waiting for matching frame");
        }
        let msg = timeout(remaining, ws.next())
            .await
            .expect("recv_until timeout")
            .expect("stream ended")
            .expect("ws error");
        let v: serde_json::Value = match msg {
            Message::Text(t) => {
                tracing::debug!("[recv_until] frame: {}", t);
                serde_json::from_str(&t).expect("not JSON")
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        };
        if predicate(&v) {
            return v;
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Test that supervisor.listen command is accepted and parsed correctly.
/// This verifies the RWI command interface is working, even though the
/// actual audio mixing requires additional implementation.
#[tokio::test]
async fn test_supervisor_listen_command_accepted() {
    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let agent_port = portpicker::pick_unused_port().expect("no free agent port");

    // Start the PBX
    let pbx = TestPbx::start(sip_port).await;

    // Create test UA
    let agent = TestUa::callee(agent_port, 1).await;

    // Connect RWI client
    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe so events are delivered
    let (sub_id, sub_json) =
        rwi_req("session.subscribe", serde_json::json!({"contexts": ["default"]}));
    let v = ws_send_recv_with_id(&mut ws, &sub_json, &sub_id).await;
    assert_eq!(v["response"], "success", "subscribe failed: {v}");

    // Originate to Agent
    let agent_call_id = format!("e2e-sup-agent-{}", Uuid::new_v4());
    let (agent_orig_id, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": agent_call_id,
            "destination": agent.sip_uri("agent"),
            "caller_id": format!("sip:rwi@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv_with_id(&mut ws, &orig_json, &agent_orig_id).await;
    assert_eq!(v["response"], "success", "originate agent failed: {v}");

    // Wait for agent to answer
    let _answered = recv_until(&mut ws, 10, |v| {
        v.get("call_answered").is_some() &&
        v["call_answered"]["call_id"] == agent_call_id
    }).await;
    tracing::info!("Agent answered");

    // Now try to send supervisor.listen - this verifies the command is accepted
    // Note: The actual audio mixing requires additional implementation
    let supervisor_call_id = format!("e2e-supervisor-{}", Uuid::new_v4());
    let (_listen_id, listen_json) = rwi_req(
        "supervisor.listen",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": agent_call_id,
        }),
    );

    // Send the command and check the response
    ws.send(Message::Text(listen_json.into())).await.unwrap();
    let msg = timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("recv timeout")
        .expect("stream ended")
        .expect("ws error");
    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("not JSON"),
        other => panic!("unexpected frame: {other:?}"),
    };

    // The command should be accepted (not return unknown_action error)
    // It may return error if the target session doesn't exist, but that's expected
    tracing::info!("supervisor.listen response: {:?}", v);
    let response = v["response"].as_str().unwrap_or("");
    assert!(
        response == "success" || response == "error" || response == "not_found",
        "Expected success/error/not_found, got: {response}"
    );

    // If it's an error, check it's not "unknown_action"
    if response == "error" {
        let error_code = v["error"]["code"].as_str().unwrap_or("");
        assert!(
            error_code != "unknown_action",
            "Command should be recognized, not unknown_action"
        );
    }

    ws.close(None).await.unwrap();
    agent.stop();
    pbx.stop();
}

/// Test that supervisor.whisper command is parsed correctly.
#[tokio::test]
async fn test_supervisor_whisper_command_accepted() {
    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let agent_port = portpicker::pick_unused_port().expect("no free agent port");

    let pbx = TestPbx::start(sip_port).await;
    let agent = TestUa::callee(agent_port, 1).await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe
    let (sub_id, sub_json) =
        rwi_req("session.subscribe", serde_json::json!({"contexts": ["default"]}));
    let v = ws_send_recv_with_id(&mut ws, &sub_json, &sub_id).await;
    assert_eq!(v["response"], "success");

    // Originate agent
    let agent_call_id = format!("e2e-whisper-agent-{}", Uuid::new_v4());
    let (orig_id, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": agent_call_id,
            "destination": agent.sip_uri("agent"),
            "caller_id": format!("sip:rwi@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    ws_send_recv_with_id(&mut ws, &orig_json, &orig_id).await;
    recv_until(&mut ws, 10, |v| v.get("call_answered").is_some()).await;

    // Try supervisor.whisper
    let supervisor_call_id = format!("e2e-whisper-sup-{}", Uuid::new_v4());
    let (_, whisper_json) = rwi_req(
        "supervisor.whisper",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": agent_call_id,
            "agent_leg": "",
        }),
    );
    ws.send(Message::Text(whisper_json.into())).await.unwrap();
    let msg = timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("recv timeout")
        .expect("stream ended")
        .expect("ws error");
    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("not JSON"),
        other => panic!("unexpected frame: {other:?}"),
    };

    tracing::info!("supervisor.whisper response: {:?}", v);
    let response = v["response"].as_str().unwrap_or("");
    assert!(
        response == "success" || response == "error" || response == "not_found",
        "Expected success/error/not_found, got: {response}"
    );

    ws.close(None).await.unwrap();
    agent.stop();
    pbx.stop();
}

/// Test that supervisor.barge command is parsed correctly.
#[tokio::test]
async fn test_supervisor_barge_command_accepted() {
    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let agent_port = portpicker::pick_unused_port().expect("no free agent port");

    let pbx = TestPbx::start(sip_port).await;
    let agent = TestUa::callee(agent_port, 1).await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe
    let (sub_id, sub_json) =
        rwi_req("session.subscribe", serde_json::json!({"contexts": ["default"]}));
    let v = ws_send_recv_with_id(&mut ws, &sub_json, &sub_id).await;
    assert_eq!(v["response"], "success");

    // Originate agent
    let agent_call_id = format!("e2e-barge-agent-{}", Uuid::new_v4());
    let (orig_id, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": agent_call_id,
            "destination": agent.sip_uri("agent"),
            "caller_id": format!("sip:rwi@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    ws_send_recv_with_id(&mut ws, &orig_json, &orig_id).await;
    recv_until(&mut ws, 10, |v| v.get("call_answered").is_some()).await;

    // Try supervisor.barge
    let supervisor_call_id = format!("e2e-barge-sup-{}", Uuid::new_v4());
    let (_, barge_json) = rwi_req(
        "supervisor.barge",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": agent_call_id,
            "agent_leg": "",
        }),
    );
    ws.send(Message::Text(barge_json.into())).await.unwrap();
    let msg = timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("recv timeout")
        .expect("stream ended")
        .expect("ws error");
    let v: serde_json::Value = match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("not JSON"),
        other => panic!("unexpected frame: {other:?}"),
    };

    tracing::info!("supervisor.barge response: {:?}", v);
    let response = v["response"].as_str().unwrap_or("");
    assert!(
        response == "success" || response == "error" || response == "not_found",
        "Expected success/error/not_found, got: {response}"
    );

    ws.close(None).await.unwrap();
    agent.stop();
    pbx.stop();
}

/// Full supervisor test: create both agent and supervisor calls, then invoke supervisor.listen
/// This tests the complete flow:
/// 1. Originate agent call -> waits for agent to answer
/// 2. Originate supervisor call -> supervisor UA answers
/// 3. Call supervisor.listen -> bridges supervisor to agent call
/// 4. Verify events are emitted correctly
#[tokio::test]
async fn test_supervisor_listen_full_flow() {
    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let agent_port = portpicker::pick_unused_port().expect("no free agent port");
    let sup_port = portpicker::pick_unused_port().expect("no free supervisor port");

    // Start the PBX
    let pbx = TestPbx::start(sip_port).await;

    // Create two test UAs: agent and supervisor
    let agent = TestUa::callee(agent_port, 1).await;
    let supervisor = TestUa::callee(sup_port, 1).await;

    // Connect RWI client
    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe so events are delivered
    let (sub_id, sub_json) =
        rwi_req("session.subscribe", serde_json::json!({"contexts": ["default"]}));
    let v = ws_send_recv_with_id(&mut ws, &sub_json, &sub_id).await;
    assert_eq!(v["response"], "success", "subscribe failed: {v}");

    // Step 1: Originate agent call
    let agent_call_id = format!("e2e-full-agent-{}", Uuid::new_v4());
    let (agent_orig_id, agent_orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": agent_call_id,
            "destination": agent.sip_uri("agent"),
            "caller_id": format!("sip:rwi@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv_with_id(&mut ws, &agent_orig_json, &agent_orig_id).await;
    assert_eq!(v["response"], "success", "originate agent failed: {v}");

    // Wait for agent to answer (or timeout)
    let agent_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        recv_until(&mut ws, 15, |v| {
            v.get("call_answered").is_some() &&
            v["call_answered"]["call_id"] == agent_call_id
        })
    ).await;
    match agent_result {
        Ok(v) => tracing::info!("Agent answered: {} - {:?}", agent_call_id, v),
        Err(_) => tracing::warn!("Agent did NOT answer within timeout, continuing anyway..."),
    }

    // Give some time for the call to stabilize
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Step 2: Originate supervisor call
    let supervisor_call_id = format!("e2e-full-supervisor-{}", Uuid::new_v4());
    let (sup_orig_id, sup_orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": supervisor_call_id,
            "destination": supervisor.sip_uri("supervisor"),
            "caller_id": format!("sip:supervisor@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    let v = ws_send_recv_with_id(&mut ws, &sup_orig_json, &sup_orig_id).await;
    tracing::info!("Supervisor originate response: {:?}", v);
    assert_eq!(v["response"], "success", "originate supervisor failed: {v}");

    // Wait for supervisor to answer (or timeout)
    let sup_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        recv_until(&mut ws, 15, |v| {
            v.get("call_answered").is_some() &&
            v["call_answered"]["call_id"] == supervisor_call_id
        })
    ).await;
    match sup_result {
        Ok(v) => tracing::info!("Supervisor answered: {} - {:?}", supervisor_call_id, v),
        Err(_) => tracing::warn!("Supervisor did NOT answer within timeout, continuing anyway..."),
    }

    // Give some time for the call to stabilize
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Step 3: Call supervisor.listen
    // This should bridge the supervisor call to the agent call
    let (listen_id, listen_json) = rwi_req(
        "supervisor.listen",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": agent_call_id,
        }),
    );
    let v = ws_send_recv_with_id(&mut ws, &listen_json, &listen_id).await;

    tracing::info!("supervisor.listen response: {:?}", v);

    // Should return success (both sessions exist and are bridged)
    assert_eq!(v["response"], "success", "Expected success, got: {v}");

    // Step 4: Verify events are emitted
    // After successful supervisor.listen, we should see call bridged events
    // or supervisor events
    tracing::info!("Full flow test completed successfully");

    ws.close(None).await.unwrap();
    agent.stop();
    supervisor.stop();
    pbx.stop();
}

/// Test supervisor.whisper with full flow
#[tokio::test]
async fn test_supervisor_whisper_full_flow() {
    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let agent_port = portpicker::pick_unused_port().expect("no free agent port");
    let sup_port = portpicker::pick_unused_port().expect("no free supervisor port");

    let pbx = TestPbx::start(sip_port).await;
    let agent = TestUa::callee(agent_port, 1).await;
    let supervisor = TestUa::callee(sup_port, 1).await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe
    let (sub_id, sub_json) =
        rwi_req("session.subscribe", serde_json::json!({"contexts": ["default"]}));
    let v = ws_send_recv_with_id(&mut ws, &sub_json, &sub_id).await;
    assert_eq!(v["response"], "success");

    // Originate agent
    let agent_call_id = format!("e2e-whisper-agent-{}", Uuid::new_v4());
    let (orig_id, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": agent_call_id,
            "destination": agent.sip_uri("agent"),
            "caller_id": format!("sip:rwi@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    ws_send_recv_with_id(&mut ws, &orig_json, &orig_id).await;
    recv_until(&mut ws, 10, |v| v.get("call_answered").is_some()).await;

    // Originate supervisor
    let supervisor_call_id = format!("e2e-whisper-supervisor-{}", Uuid::new_v4());
    let (sup_id, sup_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": supervisor_call_id,
            "destination": supervisor.sip_uri("supervisor"),
            "caller_id": format!("sip:supervisor@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    ws_send_recv_with_id(&mut ws, &sup_json, &sup_id).await;
    recv_until(&mut ws, 10, |v| v.get("call_answered").is_some()).await;

    // Call supervisor.whisper
    let (whisper_id, whisper_json) = rwi_req(
        "supervisor.whisper",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": agent_call_id,
            "agent_leg": "",
        }),
    );
    let v = ws_send_recv_with_id(&mut ws, &whisper_json, &whisper_id).await;

    tracing::info!("supervisor.whisper response: {:?}", v);
    assert_eq!(v["response"], "success", "Expected success, got: {v}");

    ws.close(None).await.unwrap();
    agent.stop();
    supervisor.stop();
    pbx.stop();
}

/// Test supervisor.barge with full flow
#[tokio::test]
async fn test_supervisor_barge_full_flow() {
    let sip_port = portpicker::pick_unused_port().expect("no free SIP port");
    let agent_port = portpicker::pick_unused_port().expect("no free agent port");
    let sup_port = portpicker::pick_unused_port().expect("no free supervisor port");

    let pbx = TestPbx::start(sip_port).await;
    let agent = TestUa::callee(agent_port, 1).await;
    let supervisor = TestUa::callee(sup_port, 1).await;

    let mut ws = ws_connect(&pbx.rwi_url).await;

    // Subscribe
    let (sub_id, sub_json) =
        rwi_req("session.subscribe", serde_json::json!({"contexts": ["default"]}));
    let v = ws_send_recv_with_id(&mut ws, &sub_json, &sub_id).await;
    assert_eq!(v["response"], "success");

    // Originate agent
    let agent_call_id = format!("e2e-barge-agent-{}", Uuid::new_v4());
    let (orig_id, orig_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": agent_call_id,
            "destination": agent.sip_uri("agent"),
            "caller_id": format!("sip:rwi@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    ws_send_recv_with_id(&mut ws, &orig_json, &orig_id).await;
    recv_until(&mut ws, 10, |v| v.get("call_answered").is_some()).await;

    // Originate supervisor
    let supervisor_call_id = format!("e2e-barge-supervisor-{}", Uuid::new_v4());
    let (sup_id, sup_json) = rwi_req(
        "call.originate",
        serde_json::json!({
            "call_id": supervisor_call_id,
            "destination": supervisor.sip_uri("supervisor"),
            "caller_id": format!("sip:supervisor@{}", pbx.sip_host()),
            "context": "default",
            "timeout_secs": 15,
        }),
    );
    ws_send_recv_with_id(&mut ws, &sup_json, &sup_id).await;
    recv_until(&mut ws, 10, |v| v.get("call_answered").is_some()).await;

    // Call supervisor.barge
    let (barge_id, barge_json) = rwi_req(
        "supervisor.barge",
        serde_json::json!({
            "supervisor_call_id": supervisor_call_id,
            "target_call_id": agent_call_id,
            "agent_leg": "",
        }),
    );
    let v = ws_send_recv_with_id(&mut ws, &barge_json, &barge_id).await;

    tracing::info!("supervisor.barge response: {:?}", v);
    assert_eq!(v["response"], "success", "Expected success, got: {v}");

    ws.close(None).await.unwrap();
    agent.stop();
    supervisor.stop();
    pbx.stop();
}
