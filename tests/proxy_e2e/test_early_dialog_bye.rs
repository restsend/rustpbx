//! A caller hanging up a ringing call with BYE on the early dialog.
//!
//! RFC 3261 §15 lets the caller end an early dialog with BYE instead of
//! CANCEL. The session must end the call, and the caller's INVITE, still
//! without a final response, must get its 487 at once (§15.1.2). It used to
//! get none: an upstream proxy only gave up after Timer C (3 min), a direct
//! caller never.

use super::raw_trunk_caller::{Harness, header, ok_to};
use crate::common::test_helpers;
use crate::common::test_ua::TestUaEvent;
use crate::common::wait::wait_for_value;
use anyhow::{Result, anyhow};
use std::time::Duration;

#[tokio::test]
async fn caller_bye_on_early_dialog_gets_487_for_its_invite() -> Result<()> {
    let h = Harness::start().await?;
    let bob = h.ua("bob", "password456").await?;
    let caller_port = h.caller.local_addr()?.port();
    let call_id = format!("early-bye-{caller_port}@test");
    let from = format!("<sip:15550001@127.0.0.1:{caller_port}>;tag=earlybye");
    let sdp = test_helpers::pcmu_sdp("127.0.0.1", 40000);
    let invite = format!(
        "INVITE sip:bob@{proxy} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-earlybye;rport\r\n\
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
    h.caller.send_to(invite.as_bytes(), h.proxy_addr).await?;

    // bob rings and never answers.
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
    bob.ring_call(&bob_dialog).await?;

    // The 180 carries the To-tag that establishes the early dialog.
    let ringing = h.collect(Duration::from_secs(1)).await?;
    let (ringing, _) = ringing
        .iter()
        .find(|(m, _)| m.starts_with("SIP/2.0 180") && header(m, "To").contains(";tag="))
        .ok_or_else(|| anyhow!("the caller never got a 180 with a To-tag"))?;
    let contact = header(ringing, "Contact");
    let target = contact
        .trim_start_matches('<')
        .split('>')
        .next()
        .unwrap_or(&contact);
    let bye = format!(
        "BYE {target} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-earlybye-bye;rport\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 2 BYE\r\n\
         Content-Length: 0\r\n\r\n",
        to = header(ringing, "To"),
    );
    h.caller.send_to(bye.as_bytes(), h.proxy_addr).await?;

    let after = h.collect(Duration::from_secs(2)).await?;
    let response = |method: &str, code: &str| {
        after.iter().find(|(m, _)| {
            m.starts_with(&format!("SIP/2.0 {code}")) && header(m, "CSeq").ends_with(method)
        })
    };
    assert!(
        response("BYE", "200").is_some(),
        "the BYE is answered: {after:?}"
    );
    let (terminated, src) = response("INVITE", "487")
        .ok_or_else(|| anyhow!("the INVITE must get its 487 at once: {after:?}"))?;
    // ACK the 487 so the server transaction completes.
    let ack = ok_to(terminated).replacen("SIP/2.0 200 OK", "ACK sip:bob@127.0.0.1 SIP/2.0", 1);
    h.caller
        .send_to(ack.replace("CSeq: 1 INVITE", "CSeq: 1 ACK").as_bytes(), src)
        .await?;

    let records = h
        .cdr
        .wait_for_records(1, Duration::from_secs(5))
        .await
        .ok_or_else(|| anyhow!("no CDR"))?;
    assert!(
        records[0].answer_time.is_none(),
        "the call was never answered"
    );
    Ok(())
}
