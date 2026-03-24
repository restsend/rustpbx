// tests/helpers/sipbot_helper.rs
//
// Thin wrapper around `sipbot::sip::SipBot` that makes it easy to spin up a
// callee SIP UA in integration tests.
//
// Typical usage:
//
// ```
// let bob = TestUa::callee("127.0.0.1", bob_sip_port, 2).await;
// // bob will ring for 2 seconds then auto-answer with echo
// // ...
// bob.stop();
// ```

use sipbot::{
    config::{AccountConfig, AnswerConfig, Config as SipBotConfig, RingConfig},
    sip::SipBot,
    stats::CallStats,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A SIP UA backed by `sipbot`.
pub struct TestUa {
    pub cancel_token: CancellationToken,
    pub domain: String,
}

impl TestUa {
    /// Create and start a callee UA that rings for `ring_secs` seconds then
    /// auto-answers with echo.  The UA binds UDP on `127.0.0.1:<sip_port>`.
    ///
    /// The underlying `SipBot::run_wait()` loop is spawned in a background
    /// task.  Drop (or call `stop()`) to terminate it.
    pub async fn callee(sip_port: u16, ring_secs: u64) -> Self {
        let cancel_token = CancellationToken::new();
        let domain = format!("127.0.0.1:{}", sip_port);

        let account = AccountConfig {
            username: "bob".to_string(),
            domain: domain.clone(),
            password: None,
            register: Some(false), // no registration needed in test
            ring: Some(RingConfig {
                duration_secs: ring_secs,
                ringback: None,
                local: None,
            }),
            answer: Some(AnswerConfig::Echo),
            ..Default::default()
        };

        let global_config = SipBotConfig {
            addr: Some(format!("127.0.0.1:{}", sip_port)),
            external_ip: None,
            recorders: None,
            accounts: vec![account.clone()],
        };

        let stats = Arc::new(CallStats::new());
        let ct = cancel_token.clone();

        tokio::spawn(async move {
            let mut bot = SipBot::new(account, global_config, stats, false, ct.clone());
            tokio::select! {
                _ = ct.cancelled() => {}
                res = bot.run_wait() => {
                    if let Err(e) = res {
                        tracing::error!("sipbot run_wait error: {e:?}");
                    }
                }
            }
        });

        // Give the UA a moment to bind its UDP socket before we start calling it.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        Self {
            cancel_token,
            domain,
        }
    }

    /// Cancel the background SipBot task.
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }

    /// Return a `sip:<user>@<host>:<port>` URI for this UA.
    pub fn sip_uri(&self, user: &str) -> String {
        format!("sip:{}@{}", user, self.domain)
    }
}

impl Drop for TestUa {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
