mod helpers;

use helpers::rwi_collector::RwiCollector;
use helpers::sipbot_helper::TestUa;
use helpers::test_server::{TEST_TOKEN, TestPbx};
use uuid::Uuid;

#[tokio::test]
async fn test_rwi_lifecycle_answer_hold_hangup() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;

    let bob_port = portpicker::pick_unused_port().expect("no free port");
    let bob = TestUa::callee_with_username(bob_port, 1, "bob").await;

    let mut rwi = RwiCollector::connect(&pbx.rwi_url, TEST_TOKEN).await;

    let subscribe = rwi
        .send_command(
            "session.subscribe",
            serde_json::json!({"contexts": ["default"]}),
        )
        .await;
    assert_eq!(subscribe["status"], "success");

    let call_id = Uuid::new_v4().to_string();
    let dest = format!("sip:bob@127.0.0.1:{}", bob_port);
    let originate = rwi
        .send_command(
            "call.originate",
            serde_json::json!({"call_id": call_id, "destination": dest, "caller_id": "alice"}),
        )
        .await;
    assert_eq!(originate["status"], "success");

    let ringing = rwi
        .wait_for_event_type("call_ringing", 15)
        .await
        .expect("missing call_ringing");
    assert!(ringing.to_string().contains(&call_id));

    let answered = rwi
        .wait_for_event_type("call_answered", 15)
        .await
        .expect("missing call_answered");
    assert!(answered.to_string().contains(&call_id));

    let hold = rwi
        .send_command("call.hold", serde_json::json!({"call_id": call_id}))
        .await;
    assert_eq!(hold["status"], "success");

    let _ = rwi.wait_for_event_type("media_hold_started", 10).await;

    let unhold = rwi
        .send_command("call.unhold", serde_json::json!({"call_id": call_id}))
        .await;
    assert_eq!(unhold["status"], "success");

    let _ = rwi.wait_for_event_type("media_hold_stopped", 10).await;

    let hangup = rwi
        .send_command("call.hangup", serde_json::json!({"call_id": call_id}))
        .await;
    assert_eq!(hangup["status"], "success");

    let hangup_event = rwi
        .wait_for_event_type("call_hangup", 10)
        .await
        .expect("missing call_hangup");
    assert!(hangup_event.to_string().contains(&call_id));

    bob.stop();
}

#[tokio::test]
async fn test_rwi_lifecycle_reject_busy() {
    let _ = tracing_subscriber::fmt::try_init();
    let sip_port = portpicker::pick_unused_port().expect("no free port");
    let pbx = TestPbx::start(sip_port).await;

    let reject_port = portpicker::pick_unused_port().expect("no free port");
    let rejecting = TestUa::callee_reject(reject_port, "busybot", 486).await;

    let mut rwi = RwiCollector::connect(&pbx.rwi_url, TEST_TOKEN).await;

    let subscribe = rwi
        .send_command(
            "session.subscribe",
            serde_json::json!({"contexts": ["default"]}),
        )
        .await;
    assert_eq!(subscribe["status"], "success");

    let call_id = Uuid::new_v4().to_string();
    let dest = format!("sip:busybot@127.0.0.1:{}", reject_port);
    let originate = rwi
        .send_command(
            "call.originate",
            serde_json::json!({"call_id": call_id, "destination": dest, "caller_id": "alice"}),
        )
        .await;
    assert_eq!(originate["status"], "success");

    let _ = rwi
        .wait_for_event_type("call_ringing", 15)
        .await
        .expect("missing call_ringing");

    let busy = rwi.wait_for_event_type("call_busy", 20).await;
    let no_answer = rwi.wait_for_event_type("call_no_answer", 1).await;
    let hangup = rwi.wait_for_event_type("call_hangup", 1).await;

    let matched = busy
        .or(no_answer)
        .or(hangup)
        .expect("missing reject outcome event");
    assert!(matched.to_string().contains(&call_id));

    rejecting.stop();
}
