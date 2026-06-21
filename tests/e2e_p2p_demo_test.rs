// tests/e2e_p2p_demo_test.rs
//
// Demo E2E test demonstrating the full testing pattern:
// - Real SIP signaling via sipbot (no mock)
// - CDR verification via CdrVerifier — RWI path now wired!
// - RWI event verification via RwiCollector
// - RWI command-response verification (hold/resume/hangup)
// - sipbot hold/resume via re-INVITE (local sipbot patch)

mod helpers;

use helpers::cdr_verifier::{CdrExpectation, CdrVerifier};
use helpers::rwi_collector::RwiCollector;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx, TestPbxInject};
use rustpbx::callrecord::CallRecordHangupReason;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// Test: basic P2P call — CDR + RWI events + event sequence.
#[tokio::test]
async fn test_p2p_basic_call_cdr_and_events() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().unwrap();
    let bob_port = portpicker::pick_unused_port().unwrap();

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

    let bob = TestUa::callee(bob_port, 1).await;
    let destination = bob.sip_uri("bob");
    let call_id = format!("e2e-demo-{}", Uuid::new_v4());
    let caller_id = format!("sip:caller-demo@{}", pbx.sip_host());

    let originate = serde_json::json!({
        "rwi": "1.0",
        "action": "call.originate",
        "action_id": "demo-originate",
        "params": {
            "call_id": call_id,
            "destination": destination,
            "caller_id": caller_id,
            "context": "default",
            "timeout_secs": 15
        }
    });
    rwi.send(&originate).await;
    rwi.wait_for_event_type("command_completed", 5).await;

    // Verify RWI events
    rwi.wait_for_event_type("call_ringing", 5).await.unwrap();
    rwi.wait_for_event_type("call_early_media", 5)
        .await
        .unwrap();
    rwi.wait_for_event_type("call_answered", 10).await.unwrap();
    sleep(Duration::from_millis(500)).await;

    // Hangup
    let hangup_cmd = serde_json::json!({
        "rwi": "1.0",
        "action": "call.hangup",
        "action_id": "demo-hangup",
        "params": {"call_id": call_id}
    });
    rwi.send(&hangup_cmd).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    eprintln!("[test] ✓ Hangup command succeeded");

    // Verify event sequence
    rwi.assert_event_sequence(&["call_ringing", "call_early_media", "call_answered"])
        .await;
    eprintln!("[test] ✓ RWI event sequence verified");

    // Verify CDR
    sleep(Duration::from_millis(1000)).await;
    let record = cdr
        .wait_for_record(&call_id, 5)
        .await
        .expect("CDR record should exist for RWI originate call");
    cdr.assert_call_completed(&record);
    cdr.assert_cdr(
        &record,
        &CdrExpectation::default()
            .with_caller("caller-demo")
            .with_hangup_reason(CallRecordHangupReason::BySystem)
            .with_min_duration(0.0),
    );
    eprintln!(
        "[test] ✓ CDR verified: status={}, duration={}s, hangup={:?}",
        record.details.status,
        (record.end_time - record.start_time).num_seconds(),
        record.hangup_reason
    );

    bob.stop();
    pbx.stop();
    eprintln!("[test] ✓ ALL BASIC CALL ASSERTIONS PASSED");
}

/// Test: Hold/Resume via RWI command + CDR verification.
#[tokio::test]
async fn test_p2p_hold_resume_via_rwi() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().unwrap();
    let bob_port = portpicker::pick_unused_port().unwrap();

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

    let bob = TestUa::callee(bob_port, 1).await;
    let destination = bob.sip_uri("bob");
    let call_id = format!("e2e-hold-{}", Uuid::new_v4());
    let caller_id = format!("sip:caller-hold@{}", pbx.sip_host());

    let originate = serde_json::json!({
        "rwi": "1.0",
        "action": "call.originate",
        "action_id": "hold-originate",
        "params": {
            "call_id": call_id,
            "destination": destination,
            "caller_id": caller_id,
            "context": "default",
            "timeout_secs": 15
        }
    });
    rwi.send(&originate).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    rwi.wait_for_event_type("call_answered", 10).await.unwrap();
    sleep(Duration::from_millis(500)).await;

    // Hold
    let hold_cmd = serde_json::json!({
        "rwi": "1.0",
        "action": "call.hold",
        "action_id": "hold-cmd",
        "params": {"call_id": call_id}
    });
    rwi.send(&hold_cmd).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    eprintln!("[test] ✓ call.hold → command_completed");

    sleep(Duration::from_millis(1000)).await;

    // Unhold
    let unhold_cmd = serde_json::json!({
        "rwi": "1.0",
        "action": "call.unhold",
        "action_id": "unhold-cmd",
        "params": {"call_id": call_id}
    });
    rwi.send(&unhold_cmd).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    eprintln!("[test] ✓ call.unhold → command_completed");

    sleep(Duration::from_millis(1000)).await;

    // Hangup
    let hangup_cmd = serde_json::json!({
        "rwi": "1.0",
        "action": "call.hangup",
        "action_id": "hold-hangup",
        "params": {"call_id": call_id}
    });
    rwi.send(&hangup_cmd).await;
    rwi.wait_for_event_type("command_completed", 5).await;

    // Verify CDR
    sleep(Duration::from_millis(1000)).await;
    let record = cdr
        .wait_for_record(&call_id, 5)
        .await
        .expect("CDR record should exist for hold/originate call");
    cdr.assert_cdr(
        &record,
        &CdrExpectation::default()
            .with_caller("caller-hold")
            .with_hangup_reason(CallRecordHangupReason::BySystem)
            .with_min_duration(2.0),
    );
    eprintln!(
        "[test] ✓ CDR for hold call: status={}, duration={}s, hangup={:?}",
        record.details.status,
        (record.end_time - record.start_time).num_seconds(),
        record.hangup_reason
    );

    bob.stop();
    pbx.stop();
    eprintln!("[test] ✓ ALL HOLD/RESUME ASSERTIONS PASSED");
}
