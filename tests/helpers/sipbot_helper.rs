#![allow(dead_code)]

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
    audio_quality::AudioQualityConfig,
    config::{AccountConfig, AnswerConfig, Config as SipBotConfig, HangupConfig, RingConfig},
    sip::SipBot,
    stats::CallStats,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct BuildCallee {
    sip_port: u16,
    username: String,
    ring_secs: u64,
    refer_reject: Option<u16>,
    record_path: Option<String>,
    reject_code: Option<u16>,
    answer: Option<Option<AnswerConfig>>,
    hangup_after_secs: Option<u64>,
}

/// A SIP UA backed by `sipbot`.
pub struct TestUa {
    pub cancel_token: CancellationToken,
    #[allow(dead_code)]
    pub domain: String,
    /// Shared call statistics for RTP validation
    #[allow(dead_code)]
    pub stats: Arc<CallStats>,
    /// WAV file path where sipbot records RX/TX audio (stereo 16kHz, L=RX, R=TX)
    #[allow(dead_code)]
    pub record_path: Option<String>,
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

    /// Create and start a REGISTERED callee UA that registers with a proxy.
    /// This is used for web agents that need to be discoverable via registrar.
    pub async fn registered_callee(
        sip_port: u16,
        ring_secs: u64,
        username: &str,
        password: &str,
        domain: &str,
        proxy_addr: &str,
    ) -> Self {
        let cancel_token = CancellationToken::new();

        let account = AccountConfig {
            username: username.to_string(),
            domain: domain.to_string(),
            password: Some(password.to_string()),
            proxy: Some(proxy_addr.to_string()),
            register: Some(true),
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
        let stats_clone = stats.clone();
        let ct = cancel_token.clone();

        rustpbx::utils::spawn(async move {
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

        // Give the UA a moment to bind its UDP socket and register.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Self {
            cancel_token,
            domain: format!("{}:{}", domain, sip_port),
            stats,
            record_path: None,
        }
    }

    /// Create a callee that immediately rejects with the given SIP code (e.g. 486 Busy, 480 Temporarily Unavailable).
    pub async fn callee_reject(sip_port: u16, username: &str, reject_code: u16) -> Self {
        Self::build_callee(BuildCallee {
            sip_port, username: username.to_string(), ring_secs: 0, refer_reject: None, record_path: None,
            reject_code: Some(reject_code), answer: None, hangup_after_secs: None,
        }).await
    }

    pub async fn callee_no_answer(sip_port: u16, username: &str, ring_secs: u64) -> Self {
        Self::build_callee(BuildCallee {
            sip_port, username: username.to_string(), ring_secs, refer_reject: None, record_path: None,
            reject_code: None, answer: Some(None), hangup_after_secs: None,
        }).await
    }

    pub async fn callee_answer_then_hangup(sip_port: u16, ring_secs: u64, username: &str, after_secs: u64) -> Self {
        Self::build_callee(BuildCallee {
            sip_port, username: username.to_string(), ring_secs, refer_reject: None, record_path: None,
            reject_code: None, answer: Some(Some(AnswerConfig::Echo)), hangup_after_secs: Some(after_secs),
        }).await
    }

    async fn build_callee(opts: BuildCallee) -> Self {
        let cancel_token = CancellationToken::new();
        let domain = format!("127.0.0.1:{}", opts.sip_port);

        let answer_val = match opts.answer {
            Some(a) => a,
            None => Some(AnswerConfig::Echo),
        };

        let account = AccountConfig {
            username: opts.username,
            domain: domain.clone(),
            password: None,
            register: Some(false),
            ring: if opts.ring_secs > 0 {
                Some(RingConfig { duration_secs: opts.ring_secs, ringback: None, local: None })
            } else {
                None
            },
            answer: answer_val,
            refer_reject: opts.refer_reject,
            record: opts.record_path,
            reject_prob: opts.reject_code.map(|_| 100),
            hangup: opts.reject_code.map(|code| HangupConfig { code, after_secs: None }).or_else(|| opts.hangup_after_secs.map(|after| HangupConfig { code: 200, after_secs: Some(after) })),
            audio_quality: Some(AudioQualityConfig { enabled: true, ..Default::default() }),
            ..Default::default()
        };

        let global_config = SipBotConfig {
            addr: Some(format!("127.0.0.1:{}", opts.sip_port)),
            external_ip: None,
            recorders: None,
            accounts: vec![account.clone()],
        };

        let stats = Arc::new(CallStats::new());
        let stats_clone = stats.clone();
        let ct = cancel_token.clone();

        rustpbx::utils::spawn(async move {
            let mut bot = SipBot::new(account, global_config, stats_clone, false, ct.clone());
            tokio::select! {
                _ = ct.cancelled() => {}
                res = bot.run_wait() => {
                    if let Err(e) = res { tracing::error!("sipbot run_wait error: {e:?}"); }
                }
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        Self { cancel_token, domain, stats, record_path: None }
    }

    /// Create and start an outbound caller UA that will place calls to `target_uri`.
    pub async fn caller_with_target(sip_port: u16, username: &str, target_uri: String) -> Self {
        Self::caller_with_options(sip_port, username, target_uri, None).await
    }

    /// Create and start an outbound caller UA that sends DTMF after answer.
    /// `dtmf_flows` format: "1s:2,1.5s:#" (delay:digit pairs)
    pub async fn caller_with_dtmf(
        sip_port: u16,
        username: &str,
        target_uri: String,
        dtmf_flows: &str,
    ) -> Self {
        Self::caller_with_options(sip_port, username, target_uri, Some(dtmf_flows.to_string()))
            .await
    }

    async fn caller_with_options(
        sip_port: u16,
        username: &str,
        target_uri: String,
        dtmf_flows: Option<String>,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        let domain = format!("127.0.0.1:{}", sip_port);

        let account = AccountConfig {
            username: username.to_string(),
            domain: domain.clone(),
            password: None,
            register: Some(false),
            target: Some(target_uri),
            dtmf_flows,
            audio_quality: Some(AudioQualityConfig {
                enabled: true,
                ..Default::default()
            }),
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

        rustpbx::utils::spawn(async move {
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
            record_path: None,
        }
    }

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
        Self::callee_with_options_and_record(sip_port, ring_secs, username, refer_reject, None)
            .await
    }

    /// Create and start a callee UA with optional WAV recording.
    pub async fn callee_with_record(
        sip_port: u16,
        ring_secs: u64,
        username: &str,
        record_path: String,
    ) -> Self {
        Self::callee_with_options_and_record(sip_port, ring_secs, username, None, Some(record_path))
            .await
    }

    async fn callee_with_options_and_record(
        sip_port: u16,
        ring_secs: u64,
        username: &str,
        refer_reject: Option<u16>,
        record_path: Option<String>,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        let domain = format!("127.0.0.1:{}", sip_port);

        let account = AccountConfig {
            username: username.to_string(),
            domain: domain.clone(),
            password: None,
            register: Some(false),
            ring: Some(RingConfig {
                duration_secs: ring_secs,
                ringback: None,
                local: None,
            }),
            answer: Some(AnswerConfig::Echo),
            refer_reject,
            record: record_path.clone(),
            audio_quality: Some(AudioQualityConfig {
                enabled: true,
                ..Default::default()
            }),
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

        rustpbx::utils::spawn(async move {
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

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        Self {
            cancel_token,
            domain,
            stats,
            record_path,
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

    /// Get the number of received DTMF events
    #[allow(dead_code)]
    pub fn rx_dtmf_count(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.stats.rx_dtmf_events.load(Ordering::Relaxed)
    }

    /// Get the number of transmitted DTMF events
    #[allow(dead_code)]
    pub fn tx_dtmf_count(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.stats.tx_dtmf_events.load(Ordering::Relaxed)
    }

    /// Get audio quality summary for assertions
    #[allow(dead_code)]
    pub fn audio_quality_summary(&self) -> AudioQualitySummary {
        use std::sync::atomic::Ordering;
        AudioQualitySummary {
            total_frames: self
                .stats
                .audio_quality_total_frames
                .load(Ordering::Relaxed),
            silence_frames: self
                .stats
                .audio_quality_silence_frames
                .load(Ordering::Relaxed),
            clipping_frames: self
                .stats
                .audio_quality_clipping_frames
                .load(Ordering::Relaxed),
            shrill_count: self
                .stats
                .audio_quality_shrill_count
                .load(Ordering::Relaxed),
            muffled_count: self
                .stats
                .audio_quality_muffled_count
                .load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
pub struct AudioQualitySummary {
    pub total_frames: u64,
    pub silence_frames: u64,
    pub clipping_frames: u64,
    pub shrill_count: u64,
    pub muffled_count: u64,
}

impl AudioQualitySummary {
    pub fn silence_ratio(&self) -> f64 {
        if self.total_frames == 0 {
            return 1.0;
        }
        self.silence_frames as f64 / self.total_frames as f64
    }

    pub fn has_audio(&self) -> bool {
        self.total_frames > 0 && self.silence_ratio() < 0.95
    }

    pub fn clipping_ratio(&self) -> f64 {
        if self.total_frames == 0 {
            return 0.0;
        }
        self.clipping_frames as f64 / self.total_frames as f64
    }
}

impl Drop for TestUa {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}
