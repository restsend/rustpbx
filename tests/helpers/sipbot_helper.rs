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
    #[allow(dead_code)]
    pub domain: String,
    /// Shared call statistics for RTP validation
    #[allow(dead_code)]
    pub stats: Arc<CallStats>,
}

impl TestUa {
    #[allow(dead_code)]
    pub async fn callee(sip_port: u16, ring_secs: u64) -> Self {
        Self::callee_with_username(sip_port, ring_secs, "bob").await
    }

    /// Create and start a callee UA with a specific username.
    pub async fn callee_with_username(sip_port: u16, ring_secs: u64, username: &str) -> Self {
        Self::callee_with_options(sip_port, ring_secs, username, None).await
    }

    /// Create and start an outbound caller UA that will place calls to `target_uri`.
    pub async fn caller_with_target(
        sip_port: u16,
        username: &str,
        target_uri: String,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        let domain = format!("127.0.0.1:{}", sip_port);

        let account = AccountConfig {
            username: username.to_string(),
            domain: domain.clone(),
            password: None,
            register: Some(false),
            target: Some(target_uri),
            ..Default::default()
        };

        let global_config = SipBotConfig {
            addr: Some(format!("127.0.0.1:{}", sip_port)),
            external_ip: None,
            recorders: None,
            accounts: vec![account.clone()],
        };

        let stats = Arc::new(CallStats::new());
        let stats_clone = stats.clone();
        let ct = cancel_token.clone();

        tokio::spawn(async move {
            let mut bot = SipBot::new(account, global_config, stats_clone, false, ct.clone());
            tokio::select! {
                _ = ct.cancelled() => {}
                res = bot.run_call(1, 1) => {
                    if let Err(e) = res {
                        tracing::error!("sipbot run_call error: {e:?}");
                    }
                }
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        Self {
            cancel_token,
            domain,
            stats,
        }
    }

    /// Create and start a callee UA with REFER rejection (for 3PCC fallback testing).
    #[allow(dead_code)]
    pub async fn callee_with_refer_reject(
        sip_port: u16,
        ring_secs: u64,
        refer_reject_code: u16,
    ) -> Self {
        Self::callee_with_options(sip_port, ring_secs, "bob", Some(refer_reject_code)).await
    }

    /// Create and start a callee UA with configurable options.
    async fn callee_with_options(
        sip_port: u16,
        ring_secs: u64,
        username: &str,
        refer_reject: Option<u16>,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        let domain = format!("127.0.0.1:{}", sip_port);

        let account = AccountConfig {
            username: username.to_string(),
            domain: domain.clone(),
            password: None,
            register: Some(false), // no registration needed in test
            ring: Some(RingConfig {
                duration_secs: ring_secs,
                ringback: None,
                local: None,
            }),
            answer: Some(AnswerConfig::Echo),
            refer_reject,
            ..Default::default()
        };

        let global_config = SipBotConfig {
            addr: Some(format!("127.0.0.1:{}", sip_port)),
            external_ip: None,
            recorders: None,
            accounts: vec![account.clone()],
        };

        let stats = Arc::new(CallStats::new());
        let stats_clone = stats.clone();
        let ct = cancel_token.clone();

        tokio::spawn(async move {
            let mut bot = SipBot::new(account, global_config, stats_clone, false, ct.clone());
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
            stats,
        }
    }

    /// Cancel the background SipBot task.
    pub fn stop(&self) {
        self.cancel_token.cancel();
    }

    /// Return a `sip:<user>@<host>:<port>` URI for this UA.
    #[allow(dead_code)]
    pub fn sip_uri(&self, user: &str) -> String {
        format!("sip:{}@{}", user, self.domain)
    }

    /// Get RTP statistics summary as a string
    #[allow(dead_code)]
    pub fn rtp_stats_summary(&self) -> String {
        use std::sync::atomic::Ordering;
        let tx_p = self.stats.tx_packets.load(Ordering::Relaxed);
        let tx_b = self.stats.tx_bytes.load(Ordering::Relaxed);
        let rx_p = self.stats.rx_packets.load(Ordering::Relaxed);
        let rx_b = self.stats.rx_bytes.load(Ordering::Relaxed);
        format!("RX: {}p/{}b TX: {}p/{}b", rx_p, rx_b, tx_p, tx_b)
    }

    /// Check if any RTP packets were received (RX > 0)
    #[allow(dead_code)]
    pub fn has_rtp_rx(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.stats.rx_packets.load(Ordering::Relaxed) > 0
    }

    /// Check if any RTP packets were transmitted (TX > 0)
    #[allow(dead_code)]
    pub fn has_rtp_tx(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.stats.tx_packets.load(Ordering::Relaxed) > 0
    }
}

impl Drop for TestUa {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
