//! Conference E2E test — sipbot participants (via RWI originate) + RWI mgmt + CDR
//!
//! Uses RWI for conference management (required, no sipbot equivalent) and
//! sipbot callees as conference participants. CDR is verified for each
//! participant (RWI originate path now generates CDR).

mod helpers;

use helpers::cdr_verifier::{CdrExpectation, CdrVerifier};
use helpers::rwi_collector::RwiCollector;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx, TestPbxInject};
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// Three-party conference with CDR verification.
///
/// Flow:
///   1. Start PBX + CDR capture + RWI collector
///   2. Start 3 sipbot callees (Alice, Bob, Charlie)
///   3. RWI originate each callee call
///   4. RWI create conference, add all 3 participants
///   5. Verify conference lifecycle (mute/unmute)
///   6. Destroy conference
///   7. Verify CDR records for all 3 participants
#[tokio::test]
async fn test_conference_three_party_with_cdr() {
    let _ = tracing_subscriber::fmt::try_init();

    let sip_port = portpicker::pick_unused_port().unwrap();
    let alice_port = portpicker::pick_unused_port().unwrap();
    let bob_port = portpicker::pick_unused_port().unwrap();
    let carol_port = portpicker::pick_unused_port().unwrap();

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
    eprintln!("[test] RWI connected");

    // Start 3 sipbot callees
    let alice = TestUa::callee_with_username(alice_port, 1, "alice").await;
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;
    let carol = TestUa::callee_with_username(carol_port, 1, "carol").await;

    // RWI originate all 3 calls
    let participants = vec![("alice", &alice), ("bob", &bob), ("carol", &carol)];
    let mut call_ids = Vec::new();

    for (name, ua) in &participants {
        let cid = format!("conf-{}-{}", name, Uuid::new_v4());
        call_ids.push(cid.clone());
        let originate = serde_json::json!({
            "rwi": "1.0",
            "action": "call.originate",
            "action_id": format!("orig-{}", name),
            "params": {
                "call_id": cid,
                "destination": ua.sip_uri(name),
                "caller_id": format!("sip:{}@127.0.0.1:{}", name, sip_port),
                "context": "default",
                "timeout_secs": 15
            }
        });
        rwi.send(&originate).await;
        rwi.wait_for_event_type("command_completed", 5).await;
        rwi.wait_for_event_type("call_answered", 10).await;
        eprintln!("[test] ✓ {} joined call", name);
    }

    // Create conference
    let conf_id = format!("test-conf-{}", Uuid::new_v4());
    let create_conf = serde_json::json!({
        "rwi": "1.0",
        "action": "conference.create",
        "action_id": "create-conf",
        "params": {
            "conference_id": conf_id,
            "host_call_id": call_ids[0],
            "max_members": 10
        }
    });
    rwi.send(&create_conf).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    eprintln!("[test] ✓ Conference created");

    // Add participants
    for cid in &call_ids {
        let add = serde_json::json!({
            "rwi": "1.0",
            "action": "conference.add",
            "action_id": format!("add-{}", cid),
            "params": {"conference_id": conf_id, "call_id": cid}
        });
        rwi.send(&add).await;
        rwi.wait_for_event_type("command_completed", 5).await;
    }
    eprintln!("[test] ✓ All 3 participants added to conference");

    // Let audio flow briefly
    sleep(Duration::from_millis(1000)).await;

    // Test mute/unmute
    let mute_cmd = serde_json::json!({
        "rwi": "1.0",
        "action": "conference.mute",
        "action_id": "mute-bob",
        "params": {"conference_id": conf_id, "call_id": call_ids[1]}
    });
    rwi.send(&mute_cmd).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    eprintln!("[test] ✓ Bob muted");

    let unmute_cmd = serde_json::json!({
        "rwi": "1.0",
        "action": "conference.unmute",
        "action_id": "unmute-bob",
        "params": {"conference_id": conf_id, "call_id": call_ids[1]}
    });
    rwi.send(&unmute_cmd).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    eprintln!("[test] ✓ Bob unmuted");

    // Destroy conference
    let destroy = serde_json::json!({
        "rwi": "1.0",
        "action": "conference.destroy",
        "action_id": "destroy-conf",
        "params": {"conference_id": conf_id}
    });
    rwi.send(&destroy).await;
    rwi.wait_for_event_type("command_completed", 5).await;
    eprintln!("[test] ✓ Conference destroyed");

    // Hangup all calls
    for cid in &call_ids {
        let hangup = serde_json::json!({
            "rwi": "1.0",
            "action": "call.hangup",
            "action_id": format!("hangup-{}", cid),
            "params": {"call_id": cid}
        });
        rwi.send(&hangup).await;
        rwi.wait_for_event_type("command_completed", 5).await;
    }
    eprintln!("[test] ✓ All calls hung up");

    sleep(Duration::from_millis(1000)).await;

    // Verify CDR for all 3 participants
    let all_records = cdr.get_all_records().await;
    eprintln!("[test] CDR records: {}", all_records.len());
    for r in &all_records {
        eprintln!(
            "  call_id={} status={} caller={:.30}",
            r.call_id, r.details.status, r.caller
        );
    }
    assert!(!all_records.is_empty(), "should have CDR records");
    assert!(all_records.len() >= 3, "should have at least 3 CDR records");

    for cid in &call_ids {
        let record = cdr
            .wait_for_record(cid, 3)
            .await
            .unwrap_or_else(|| panic!("CDR not found for {}", cid));
        cdr.assert_cdr(
            &record,
            &CdrExpectation::default()
                .with_status("answered")
                .with_min_duration(0.5),
        );
    }
    eprintln!("[test] ✓ All 3 CDR records verified");

    alice.stop();
    bob.stop();
    carol.stop();
    pbx.stop();
    eprintln!("[test] ✓ Conference E2E test ALL PASSED");
}
