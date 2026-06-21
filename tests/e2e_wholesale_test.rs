//! Wholesale E2E test — routing + CDR billing via RWI originate + sipbot
//! Requires: --features wholesale

#![cfg(feature = "wholesale")]

mod helpers;

use helpers::cdr_verifier::CdrVerifier;
use helpers::rwi_collector::RwiCollector;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx, TestPbxInject};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// Wholesale basic routing + CDR via RWI originate
/// AddonRegistry::new() automatically includes wholesale when feature is enabled
#[tokio::test]
async fn test_wholesale_cdr_after_rwi_originate() {
    let sp = portpicker::pick_unused_port().unwrap();
    let ap = portpicker::pick_unused_port().unwrap();
    let (cdr, cs) = CdrVerifier::new();

    let pbx = TestPbx::start_with_inject(
        sp,
        TestPbxInject {
            callrecord_sender: Some(cs),
            // AddonRegistry::new() auto-registers all addons for enabled features
            addon_registry: Some(Arc::new(rustpbx::addons::registry::AddonRegistry::new())),
            ..Default::default()
        },
    )
    .await;

    let mut rwi = RwiCollector::connect(&pbx.rwi_url, TEST_TOKEN).await;
    rwi.wait_for_event_type("command_completed", 3).await;

    let agent = TestUa::callee_with_username(ap, 1, "callee").await;
    let cid = format!("ws-e2e-{}", Uuid::new_v4());

    rwi.send(
        &serde_json::json!({"rwi":"1.0","action":"call.originate","action_id":"wo","params":{
            "call_id":cid,"destination":agent.sip_uri("callee"),
            "caller_id":format!("sip:caller@{}",pbx.sip_host()),
            "context":"default","timeout_secs":15
        }}),
    )
    .await;
    rwi.wait_for_event_type("command_completed", 5).await;
    rwi.wait_for_event_type("call_answered", 10).await;
    sleep(Duration::from_millis(1000)).await;

    rwi.send(&serde_json::json!({"rwi":"1.0","action":"call.hangup","action_id":"wh","params":{"call_id":cid}})).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    sleep(Duration::from_millis(1000)).await;

    let r = cdr
        .wait_for_record(&cid, 3)
        .await
        .unwrap_or_else(|| panic!("CDR not found: {}", cid));
    cdr.assert_call_completed(&r);
    eprintln!(
        "[WS] ✓ CDR: call_id={} status={} dur={}s",
        r.call_id,
        r.details.status,
        (r.end_time - r.start_time).num_seconds()
    );

    agent.stop();
    pbx.stop();
}
