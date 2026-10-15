//! An IVR transfer hands the caller to a SIP target the SIP REFER way: the
//! target dials as an independent leg and, with no transferor to fall back
//! to, a target that rejects ends the call instead of leaving the caller on a
//! silent line.

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_ua::{TestUa, TestUaEvent, create_test_sdp};
use crate::common::wait::{wait_for_value, wait_until};
use anyhow::Result;
use rsipstack::dialog::DialogId;
use rustpbx::config::ProxyConfig;
use rustpbx::proxy::routing::{MatchConditions, RouteAction, RouteRule};
use std::time::Duration;

const IVR_NUMBER: &str = "8890";

/// IVR name, also its file name in the IVR directory (return-app lookup).
const IVR_NAME: &str = "handoffivr";

/// Key 1 runs `transfer_action`; key 2 hangs up. The greeting is missing, so
/// the menu waits for DTMF at once (no audio dependency).
fn write_ivr_dir(transfer_action: &str) -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "ivr-handoff-{}",
        portpicker::pick_unused_port().unwrap_or(43000)
    ));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(format!("{IVR_NAME}.toml")),
        format!(
            r#"
[ivr]
name = "{IVR_NAME}"

[ivr.root]
greeting = "sounds/definitely-missing-menu.wav"
timeout_ms = 10000
max_retries = 3

[[ivr.root.entries]]
key = "1"
action = {transfer_action}

[[ivr.root.entries]]
key = "2"
action = {{ type = "hangup" }}
"#
        ),
    )?;
    Ok(dir)
}

async fn start_server(transfer_action: &str) -> Result<E2eTestServer> {
    let ivr_dir = write_ivr_dir(transfer_action)?;
    let ivr_file = ivr_dir.join(format!("{IVR_NAME}.toml"));
    E2eTestServer::start_with_config(ProxyConfig {
        ivr_dir: Some(ivr_dir.to_string_lossy().to_string()),
        routes: Some(vec![RouteRule {
            name: "ivr-handoff".to_string(),
            priority: 10,
            match_conditions: MatchConditions {
                to_user: Some(IVR_NUMBER.to_string()),
                ..Default::default()
            },
            action: RouteAction {
                app: Some("ivr".to_string()),
                app_params: Some(serde_json::json!({ "file": ivr_file.to_string_lossy() })),
                ..Default::default()
            },
            ..Default::default()
        }]),
        ..Default::default()
    })
    .await
}

/// Call the IVR as alice, press 1 and return the answered dialog.
async fn enter_ivr_and_press_one(alice: &TestUa) -> Result<DialogId> {
    let dialog = alice
        .make_call(IVR_NUMBER, Some(create_test_sdp("127.0.0.1", 12345, false)))
        .await?;
    // The IVR answered in on_enter, so it already takes DTMF (barge-in).
    alice.send_dtmf_info(&dialog, "1").await?;
    Ok(dialog)
}

/// Wait for bob's incoming transfer leg.
async fn wait_incoming(bob: &TestUa) -> Result<DialogId> {
    wait_for_value(Duration::from_secs(5), || async {
        bob.process_dialog_events().await.ok()?.into_iter().find_map(|event| match event {
            TestUaEvent::IncomingCall(id, _) => Some(id),
            _ => None,
        })
    })
    .await
    .map_err(|_| anyhow::anyhow!("bob never received the transfer"))
}

/// Whether alice's call ends within `within`.
async fn alice_terminated(alice: &TestUa, within: Duration) -> Result<bool> {
    Ok(wait_until(within, || async {
        alice.process_dialog_events().await.is_ok_and(|events| {
            events
                .iter()
                .any(|event| matches!(event, TestUaEvent::CallTerminated(_)))
        })
    })
    .await
    .is_ok())
}

const TRANSFER_TO_BOB: &str = r#"{ type = "transfer", target = "bob" }"#;

#[tokio::test]
async fn ivr_transfer_rejected_by_target_hangs_up_caller() -> Result<()> {
    let server = start_server(TRANSFER_TO_BOB).await?;
    let alice = server.create_ua("alice").await?;
    let bob = server.create_ua("bob").await?;

    enter_ivr_and_press_one(&alice).await?;
    let incoming = wait_incoming(&bob).await?;
    bob.reject_call(&incoming).await?;

    assert!(
        alice_terminated(&alice, Duration::from_secs(5)).await?,
        "a rejected IVR transfer must not leave the caller on a silent line"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn ivr_transfer_answered_by_target_keeps_caller_connected() -> Result<()> {
    let server = start_server(TRANSFER_TO_BOB).await?;
    let alice = server.create_ua("alice").await?;
    let bob = server.create_ua("bob").await?;

    let dialog = enter_ivr_and_press_one(&alice).await?;
    let incoming = wait_incoming(&bob).await?;
    bob.answer_call(&incoming, Some(create_test_sdp("127.0.0.1", 23456, false)))
        .await?;

    assert!(
        !alice_terminated(&alice, Duration::from_secs(2)).await?,
        "an answered IVR transfer must keep the caller connected"
    );

    alice.hangup(&dialog).await?;
    server.stop();
    Ok(())
}

#[tokio::test]
async fn ivr_transfer_rejected_with_return_app_returns_caller_to_ivr() -> Result<()> {
    let server = start_server(&format!(
        r#"{{ type = "transfer", target = "bob", return_app = "ivr", return_target = "{IVR_NAME}" }}"#
    ))
    .await?;
    let alice = server.create_ua("alice").await?;
    let bob = server.create_ua("bob").await?;

    let dialog = enter_ivr_and_press_one(&alice).await?;
    let incoming = wait_incoming(&bob).await?;
    bob.reject_call(&incoming).await?;

    // Back in the IVR: still connected, and its menu answers key 2.
    assert!(
        !alice_terminated(&alice, Duration::from_secs(2)).await?,
        "the IVR's return_app must take the caller back instead of hanging up"
    );
    alice.send_dtmf_info(&dialog, "2").await?;
    assert!(
        alice_terminated(&alice, Duration::from_secs(5)).await?,
        "the returned IVR should hang up on key 2"
    );

    server.stop();
    Ok(())
}
