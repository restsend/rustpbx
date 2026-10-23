//! P-Early-Media (RFC 5009) from an internal callee's 183 to the caller's 183.
//!
//! Gated carriers only let early media through to their subscriber when the
//! 18x authorizes it. A callee's `P-Early-Media` is a custom header, and
//! internal callees copy every custom header both ways, so it reaches the
//! caller unchanged; trunks need it in their `header_passthrough` rule.

use crate::common::e2e_test_server::E2eTestServer;
use crate::common::test_helpers::pcmu_sdp;
use crate::common::test_ua::TestUaEvent;
use crate::common::wait::{wait_for_value, wait_until};
use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use std::sync::Arc;
use std::time::Duration;

/// Places alice -> bob, has bob answer the INVITE with a 183 + SDP carrying
/// `callee_headers`, then returns the `P-Early-Media` values alice saw on 1xx.
async fn p_early_media_seen_by_caller(
    mode: MediaProxyMode,
    caller_headers: Vec<(&str, &str)>,
    callee_headers: Vec<(&str, &str)>,
) -> Result<Vec<String>> {
    let server = Arc::new(E2eTestServer::start_with_mode(mode).await?);
    let caller_ua = Arc::new(server.create_ua("alice").await?);
    let callee_ua = server.create_ua("bob").await?;

    let to_owned = |headers: Vec<(&str, &str)>| -> Vec<(String, String)> {
        headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    };
    let caller_headers = to_owned(caller_headers);
    let callee_headers = to_owned(callee_headers);

    let caller_clone = caller_ua.clone();
    let caller_sdp = pcmu_sdp("127.0.0.1", 40000);
    let caller_handle = rustpbx::utils::spawn(async move {
        caller_clone
            .make_call_with_headers("bob", Some(caller_sdp), caller_headers)
            .await
    });

    let callee_id = wait_for_value(Duration::from_secs(5), || async {
        callee_ua
            .process_dialog_events()
            .await
            .ok()?
            .into_iter()
            .find_map(|event| match event {
                TestUaEvent::IncomingCall(id, _) => Some(id),
                _ => None,
            })
    })
    .await
    .map_err(|_| anyhow::anyhow!("bob never got the INVITE"))?;
    callee_ua
        .send_ringing_with_headers(
            &callee_id,
            Some(pcmu_sdp("127.0.0.1", 40010)),
            callee_headers,
        )
        .await?;

    // Answer only once alice has the 183, so its headers are what she saw.
    wait_until(Duration::from_secs(5), || async {
        !caller_ua.provisional_header_values("CSeq").await.is_empty()
    })
    .await
    .map_err(|_| anyhow::anyhow!("alice never got the 183"))?;
    callee_ua
        .answer_call(&callee_id, Some(pcmu_sdp("127.0.0.1", 40010)))
        .await?;

    let caller_id = tokio::time::timeout(Duration::from_secs(8), caller_handle)
        .await
        .map_err(|_| anyhow::anyhow!("alice's INVITE timed out"))?
        .map_err(|e| anyhow::anyhow!("join: {}", e))?
        .map_err(|e| anyhow::anyhow!("call: {}", e))?;

    let seen = caller_ua.provisional_header_values("P-Early-Media").await;

    caller_ua.hangup(&caller_id).await.ok();
    server.stop();
    Ok(seen)
}

#[tokio::test]
async fn test_p_early_media_relayed_to_gated_caller_anchored() -> Result<()> {
    let seen = p_early_media_seen_by_caller(
        MediaProxyMode::All,
        vec![("P-Early-Media", "supported,gated")],
        vec![("P-Early-Media", "sendrecv")],
    )
    .await?;
    assert_eq!(seen, vec!["sendrecv".to_string()]);
    Ok(())
}

#[tokio::test]
async fn test_p_early_media_relayed_to_gated_caller_unanchored() -> Result<()> {
    let seen = p_early_media_seen_by_caller(
        MediaProxyMode::None,
        vec![("P-Early-Media", "supported,gated")],
        vec![("P-Early-Media", "sendrecv")],
    )
    .await?;
    assert_eq!(seen, vec!["sendrecv".to_string()]);
    Ok(())
}

#[tokio::test]
async fn test_p_early_media_passed_through_from_internal_callee() -> Result<()> {
    // No caller support signalled: the internal pass-all rule still copies
    // the callee's header, like any other custom header.
    let seen = p_early_media_seen_by_caller(
        MediaProxyMode::All,
        vec![],
        vec![("P-Early-Media", "sendrecv")],
    )
    .await?;
    assert_eq!(seen, vec!["sendrecv".to_string()]);
    Ok(())
}

#[tokio::test]
async fn test_p_early_media_not_invented_without_callee_authorization() -> Result<()> {
    let seen = p_early_media_seen_by_caller(
        MediaProxyMode::All,
        vec![("P-Early-Media", "supported,gated")],
        vec![],
    )
    .await?;
    assert!(seen.is_empty(), "unexpected P-Early-Media: {seen:?}");
    Ok(())
}
