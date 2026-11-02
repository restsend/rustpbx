//! A trunk's `max_call_duration` hangs the call up when it is reached.
//!
//! The limit counts from the start of the call. Both legs must get their BYE
//! right then: the timer used to only cancel the session, and the BYEs went
//! out after the 3 s shutdown drain.

use super::raw_trunk_caller::{Harness, angle_uri, header, ok_to};
use crate::common::test_helpers;
use crate::common::test_ua::TestUaEvent;
use crate::common::wait::wait_for_value;
use anyhow::{Result, anyhow};
use rustpbx::callrecord::CallRecordHangupReason;
use std::time::Duration;
use tokio::time::Instant;

const LIMIT: Duration = Duration::from_secs(2);

#[tokio::test]
async fn trunk_max_call_duration_hangs_up_both_legs_at_the_limit() -> Result<()> {
    let h = Harness::start_with_trunk(|trunk| {
        trunk.max_call_duration = Some(LIMIT.as_secs() as u32);
    })
    .await?;
    let bob = h.ua("bob", "password456").await?;
    let caller_port = h.caller.local_addr()?.port();
    let call_id = format!("max-dur-{caller_port}@test");
    // A foreign host: a trunk call from our own realm counts as outbound,
    // and only inbound calls carry their source trunk.
    let from = "<sip:15550001@carrier.example>;tag=maxdur";
    let sdp = test_helpers::pcmu_sdp("127.0.0.1", 40000);
    let invite = format!(
        "INVITE sip:bob@{proxy} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-maxdur;rport\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: <sip:bob@127.0.0.1>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:15550001@127.0.0.1:{caller_port}>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {len}\r\n\r\n{sdp}",
        proxy = h.proxy_addr,
        len = sdp.len(),
    );
    let started = Instant::now();
    h.caller.send_to(invite.as_bytes(), h.proxy_addr).await?;

    let bob_dialog = wait_for_value(Duration::from_secs(5), || async {
        bob.process_dialog_events()
            .await
            .ok()?
            .into_iter()
            .find_map(|event| match event {
                TestUaEvent::IncomingCall(id, _) => Some(id),
                _ => None,
            })
    })
    .await
    .map_err(|_| anyhow!("bob never received the INVITE"))?;
    bob.answer_call(
        &bob_dialog,
        Some(test_helpers::pcmu_sdp("127.0.0.1", 40010)),
    )
    .await?;

    let answered = h.collect(Duration::from_millis(300)).await?;
    let ok = answered
        .iter()
        .map(|(m, _)| m)
        .find(|m| m.starts_with("SIP/2.0 200") && header(m, "CSeq").ends_with("INVITE"))
        .ok_or_else(|| anyhow!("the caller never got the 200 OK"))?
        .clone();
    let ack = format!(
        "ACK {target} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-maxdur-ack;rport\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n",
        target = angle_uri(&header(&ok, "Contact")),
        to = header(&ok, "To"),
    );
    h.caller.send_to(ack.as_bytes(), h.proxy_addr).await?;

    // Wait for the BYE, answering it at once.
    let mut buf = vec![0u8; 65535];
    let bye_at = loop {
        let (n, src) = tokio::time::timeout(LIMIT * 4, h.caller.recv_from(&mut buf))
            .await
            .map_err(|_| anyhow!("the caller never got a BYE"))??;
        let msg = String::from_utf8_lossy(&buf[..n]).to_string();
        if msg.starts_with("BYE ") {
            let bye_at = started.elapsed();
            h.caller.send_to(ok_to(&msg).as_bytes(), src).await?;
            break bye_at;
        }
    };
    assert!(
        bye_at >= LIMIT && bye_at < LIMIT + Duration::from_millis(800),
        "the caller's BYE goes out at the limit, not after the shutdown drain: {bye_at:?}"
    );

    wait_for_value(Duration::from_secs(2), || async {
        bob.process_dialog_events()
            .await
            .ok()?
            .into_iter()
            .find(|event| matches!(event, TestUaEvent::CallTerminated(_)))
    })
    .await
    .map_err(|_| anyhow!("bob's leg was not hung up"))?;

    let records = h
        .cdr
        .wait_for_records(1, Duration::from_secs(5))
        .await
        .ok_or_else(|| anyhow!("no CDR"))?;
    let record = &records[0];
    assert_eq!(
        record.hangup_reason,
        Some(CallRecordHangupReason::Autohangup)
    );
    let length = record.end_time - record.start_time;
    assert!(
        length >= chrono::Duration::from_std(LIMIT)?
            && length < chrono::Duration::milliseconds(2800),
        "the CDR ends at the limit: {length}"
    );
    Ok(())
}
