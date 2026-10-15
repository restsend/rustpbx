//! Calls the callee cannot take fall back to the callee's voicemail.
//!
//! These run without addons, so core's built-in voicemail app answers and
//! every known local user without `voicemail_disabled` qualifies. Voicemail
//! taking the call shows as the caller's INVITE being answered (200 OK)
//! instead of failing.

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_ua::TestUaEvent;
use crate::common::wait::wait_for_value;
use anyhow::Result;
use rustpbx::call::user::SipUser;
use rustpbx::config::ProxyConfig;
use std::sync::Arc;
use std::time::Duration;

/// Callee with voicemail on; `create_ua` registers it with password "password".
const VOICEMAIL_USER: &str = "dave";

async fn add_voicemail_user(server: &E2eTestServer) -> Result<()> {
    server
        .server_ref
        .user_backend
        .create_user(SipUser {
            id: 10,
            username: VOICEMAIL_USER.to_string(),
            password: Some("password".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            voicemail_disabled: false,
            ..Default::default()
        })
        .await
}

fn caller_sdp() -> String {
    crate::common::test_ua::create_test_sdp("127.0.0.1", 12345, false)
}

#[tokio::test]
async fn offline_callee_goes_to_voicemail() -> Result<()> {
    let server = E2eTestServer::start().await?;
    add_voicemail_user(&server).await?;
    let alice = server.create_ua("alice").await?;

    // dave never registers: routing used to reject with 480.
    let dialog = alice.make_call(VOICEMAIL_USER, Some(caller_sdp())).await?;
    alice.hangup(&dialog).await?;

    server.stop();
    Ok(())
}

#[tokio::test]
async fn offline_callee_with_voicemail_disabled_is_rejected() -> Result<()> {
    let server = E2eTestServer::start().await?;
    let alice = server.create_ua("alice").await?;

    // bob (voicemail_disabled) never registers: still a 480.
    let result = alice.make_call("bob", Some(caller_sdp())).await;
    assert!(result.is_err(), "call to an opted-out offline callee was answered");

    server.stop();
    Ok(())
}

#[tokio::test]
async fn declined_call_goes_to_voicemail() -> Result<()> {
    let server = E2eTestServer::start().await?;
    add_voicemail_user(&server).await?;
    let alice = Arc::new(server.create_ua("alice").await?);
    let dave = server.create_ua(VOICEMAIL_USER).await?;

    let call = rustpbx::utils::spawn({
        let alice = alice.clone();
        async move { alice.make_call(VOICEMAIL_USER, Some(caller_sdp())).await }
    });

    let incoming = wait_for_value(Duration::from_secs(5), || async {
        dave.process_dialog_events().await.ok()?.into_iter().find_map(|event| match event {
            TestUaEvent::IncomingCall(id, _) => Some(id),
            _ => None,
        })
    })
    .await
    .map_err(|_| anyhow::anyhow!("dave never saw the call"))?;
    dave.reject_call(&incoming).await?;

    let dialog = call.await??;
    alice.hangup(&dialog).await?;

    server.stop();
    Ok(())
}

#[tokio::test]
async fn unanswered_call_goes_to_voicemail_at_ring_timeout() -> Result<()> {
    let server = E2eTestServer::start_with_config(ProxyConfig {
        max_ring_time: Some(2),
        ..Default::default()
    })
    .await?;
    add_voicemail_user(&server).await?;
    let alice = server.create_ua("alice").await?;
    // dave registers but never answers.
    let _dave = server.create_ua(VOICEMAIL_USER).await?;

    let dialog = alice.make_call(VOICEMAIL_USER, Some(caller_sdp())).await?;
    alice.hangup(&dialog).await?;

    server.stop();
    Ok(())
}

#[tokio::test]
async fn unknown_extension_is_not_a_voicemail_destination() -> Result<()> {
    let server = E2eTestServer::start().await?;
    let alice = server.create_ua("alice").await?;
    let result = alice.make_call("nonexistent-mailbox", Some(caller_sdp())).await;
    assert!(result.is_err(), "unknown extension must not be answered by voicemail");
    server.stop();
    Ok(())
}
