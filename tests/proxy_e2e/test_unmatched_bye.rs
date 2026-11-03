//! A BYE for a dialog we no longer have gets 481 (RFC 3261 §12.2.2), which
//! also ends the dialog on the sender's side (§15.1.1), so a peer that
//! retransmits its BYE after our teardown clears instead of timing out.

use super::raw_trunk_caller::{Harness, header};
use anyhow::{Result, anyhow};
use std::time::Duration;

#[tokio::test]
async fn bye_without_a_dialog_gets_481() -> Result<()> {
    let h = Harness::start().await?;
    let caller_port = h.caller.local_addr()?.port();
    let bye = format!(
        "BYE sip:bob@{proxy} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:{caller_port};branch=z9hG4bK-orphanbye;rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:15550001@carrier.example>;tag=orphan\r\n\
         To: <sip:bob@127.0.0.1>;tag=gone\r\n\
         Call-ID: orphan-bye-{caller_port}@test\r\n\
         CSeq: 2 BYE\r\n\
         Content-Length: 0\r\n\r\n",
        proxy = h.proxy_addr,
    );
    h.caller.send_to(bye.as_bytes(), h.proxy_addr).await?;

    let replies = h.collect(Duration::from_secs(1)).await?;
    let (reply, _) = replies
        .iter()
        .find(|(m, _)| m.starts_with("SIP/2.0") && header(m, "CSeq").ends_with("BYE"))
        .ok_or_else(|| anyhow!("the BYE got no response: {replies:?}"))?;
    assert!(reply.starts_with("SIP/2.0 481"), "{reply}");
    Ok(())
}
