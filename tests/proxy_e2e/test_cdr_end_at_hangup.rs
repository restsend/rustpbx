//! The CDR ends when the call is hung up, not when teardown finishes.
//!
//! Reproduces a production report: the callee hung up 0.2 s after answering,
//! before the caller's ACK, and the caller only ACKed and answered the BYE
//! ~15 s later. The session (and so the CDR, stamped at report time) stayed
//! open until that BYE completed, billing 16 s for a 0.2 s call. Here the
//! caller is a raw UDP socket that withholds its ACK and its BYE reply.

use super::raw_trunk_caller::{Harness, angle_uri, header, ok_to};
use crate::common::test_helpers;
use crate::common::test_ua::TestUaEvent;
use crate::common::wait::wait_for_value;
use anyhow::{Result, anyhow};
use std::time::Duration;
use tokio::time::Instant;

/// How long the caller sits on its ACK and on the BYE reply.
const CALLER_DELAY: Duration = Duration::from_secs(2);

#[tokio::test]
async fn cdr_ends_at_callee_hangup_while_caller_ack_and_bye_reply_lag() -> Result<()> {
    let h = Harness::start().await?;
    let bob = h.ua("bob", "password456").await?;
    let caller_port = h.caller.local_addr()?.port();
    let call_id = format!("cdr-end-{caller_port}@test");
    let sdp = test_helpers::pcmu_sdp("127.0.0.1", 40000);
    let invite = format!(
        "INVITE sip:bob@{proxy} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-cdrend;rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:15550001@127.0.0.1>;tag=cdrend\r\n\
         To: <sip:bob@127.0.0.1>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:15550001@127.0.0.1:{caller_port}>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {len}\r\n\r\n{sdp}",
        proxy = h.proxy_addr,
        len = sdp.len(),
    );
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

    // The 200 OK reaches the caller, which does not ACK; bob hangs up.
    let answered = h.collect(Duration::from_millis(300)).await?;
    let ok = answered
        .iter()
        .map(|(m, _)| m)
        .find(|m| m.starts_with("SIP/2.0 200") && header(m, "CSeq").ends_with("INVITE"))
        .ok_or_else(|| anyhow!("the caller never got the 200 OK"))?
        .clone();
    bob.hangup(&bob_dialog).await?;
    let hung_up = Instant::now();

    // The caller sits on its ACK and on the BYE reply.
    let lagged = h.collect(CALLER_DELAY).await?;
    let (bye, bye_src) = lagged
        .iter()
        .find(|(m, _)| m.starts_with("BYE "))
        .ok_or_else(|| anyhow!("the proxy never sent the caller a BYE"))?
        .clone();
    let ack = format!(
        "ACK {target} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-cdrend-ack;rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:15550001@127.0.0.1>;tag=cdrend\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n",
        target = angle_uri(&header(&ok, "Contact")),
        to = header(&ok, "To"),
    );
    h.caller.send_to(ack.as_bytes(), h.proxy_addr).await?;
    h.caller.send_to(ok_to(&bye).as_bytes(), bye_src).await?;

    let records = h
        .cdr
        .wait_for_records(1, Duration::from_secs(10))
        .await
        .ok_or_else(|| anyhow!("no CDR"))?;
    assert!(
        hung_up.elapsed() >= CALLER_DELAY,
        "the CDR may only be written once the caller's BYE completed"
    );
    let record = &records[0];
    let answer = record.answer_time.expect("an answered call");
    let duration = record.end_time - answer;
    assert!(
        duration < chrono::Duration::milliseconds(800),
        "the call lasted from answer to bob's hangup, not until the caller's late BYE reply: {duration}"
    );
    assert!(duration >= chrono::Duration::zero());
    Ok(())
}
