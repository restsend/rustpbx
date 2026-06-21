//! CC E2E test — agent callout + CDR via RWI originate
//! Requires: --features addon-cc

#![cfg(feature = "addon-cc")]

mod helpers;

use helpers::cdr_verifier::CdrVerifier;
use helpers::rwi_collector::RwiCollector;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx, TestPbxInject};
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// Agent callout via RWI originate → CDR verification
#[tokio::test]
async fn test_cc_agent_callout_cdr() {
    let sip_port = portpicker::pick_unused_port().unwrap();
    let agent_port = portpicker::pick_unused_port().unwrap();
    let (cdr, cdr_sender) = CdrVerifier::new();

    let pbx = TestPbx::start_with_inject(
        sip_port,
        TestPbxInject {
            callrecord_sender: Some(cdr_sender),
            ..Default::default()
        },
    )
    .await;

    let mut rwi = RwiCollector::connect(&pbx.rwi_url, TEST_TOKEN).await;
    rwi.wait_for_event_type("command_completed", 3).await;

    let agent = TestUa::callee_with_username(agent_port, 1, "agent1001").await;
    let call_id = format!("cc-callout-{}", Uuid::new_v4());

    rwi.send(
        &serde_json::json!({"rwi":"1.0","action":"call.originate","action_id":"cc-orig","params":{
            "call_id":call_id,"destination":agent.sip_uri("agent1001"),
            "caller_id":format!("sip:cc-system@{}",pbx.sip_host()),
            "context":"default","timeout_secs":15
        }}),
    )
    .await;
    rwi.wait_for_event_type("command_completed", 5).await;
    rwi.wait_for_event_type("call_answered", 10).await;

    sleep(Duration::from_millis(1000)).await;

    rwi.send(&serde_json::json!({"rwi":"1.0","action":"call.hangup","action_id":"cc-hup","params":{"call_id":call_id}})).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    sleep(Duration::from_millis(1000)).await;

    let r = cdr.wait_for_record(&call_id, 3).await.expect("CC CDR");
    cdr.assert_call_completed(&r);
    eprintln!(
        "[CC] ✓ Agent callout CDR: status={} dur={}s",
        r.details.status,
        (r.end_time - r.start_time).num_seconds()
    );

    agent.stop();
    pbx.stop();
}
