//! Regression tests for the CallApp framework.
//!
//! All tests run against [`MockCallStack`] — no SIP socket, DB, or real media.

#[cfg(test)]
mod tests {
    use crate::call::app::testing::MockCallStack;
    use crate::call::app::{
        AppAction, ApplicationContext, CallApp, CallAppType, CallController, DtmfCollectConfig,
    };
    use crate::proxy::proxy_call::state::SessionAction;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // ── shared helpers ────────────────────────────────────────────────────────

    /// Log of events seen by an app, used to assert execution order.
    type EventLog = Arc<Mutex<Vec<String>>>;

    fn new_log() -> EventLog {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn logged(log: &EventLog) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    // ── 1. Basic lifecycle ────────────────────────────────────────────────────

    /// App that answers, plays one audio clip, then hangs up when playback ends.
    struct GreetAndHangupApp;

    #[async_trait]
    impl CallApp for GreetAndHangupApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Custom
        }
        fn name(&self) -> &str {
            "greet-and-hangup"
        }

        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            ctrl.answer().await?;
            ctrl.play_audio("sounds/hello.wav", false).await?;
            Ok(AppAction::Continue)
        }

        async fn on_audio_complete(
            &mut self,
            _track_id: String,
            _ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            Ok(AppAction::Hangup { reason: None })
        }
    }

    #[tokio::test]
    async fn test_basic_lifecycle() {
        let mut stack = MockCallStack::run(Box::new(GreetAndHangupApp), "1001", "1002");

        // App should immediately answer on enter
        stack
            .assert_cmd(100, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // App should play a prompt
        stack
            .assert_cmd(100, "PlayPrompt", |c| {
                matches!(c, SessionAction::PlayPrompt { .. })
            })
            .await;

        // Simulate audio finishing — app should hang up
        stack.audio_complete("default");
        stack
            .assert_cmd(100, "Hangup", |c| matches!(c, SessionAction::Hangup { .. }))
            .await;
    }

    // ── 2. DTMF routing ───────────────────────────────────────────────────────

    struct DtmfMenuApp;

    #[async_trait]
    impl CallApp for DtmfMenuApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Ivr
        }
        fn name(&self) -> &str {
            "dtmf-menu"
        }

        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            ctrl.answer().await?;
            Ok(AppAction::Continue)
        }

        async fn on_dtmf(
            &mut self,
            digit: String,
            _ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            match digit.as_str() {
                "1" => Ok(AppAction::Transfer("sip:sales@pbx".to_string())),
                "9" => Ok(AppAction::Hangup { reason: None }),
                _ => Ok(AppAction::Continue),
            }
        }
    }

    #[tokio::test]
    async fn test_dtmf_ignored_digit() {
        let mut stack = MockCallStack::run(Box::new(DtmfMenuApp), "1001", "8000");
        stack
            .assert_cmd(100, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // Press an unmapped digit → no new command
        stack.dtmf("5");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            stack.drain_cmds().is_empty(),
            "expected no command for unmapped digit"
        );
    }

    #[tokio::test]
    async fn test_dtmf_hangup_digit() {
        let mut stack = MockCallStack::run(Box::new(DtmfMenuApp), "1001", "8000");
        stack
            .assert_cmd(100, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        stack.dtmf("9");
        stack
            .assert_cmd(100, "Hangup", |c| matches!(c, SessionAction::Hangup { .. }))
            .await;
    }

    // ── 3. Remote hangup handled gracefully ───────────────────────────────────

    struct WaitForeverApp;

    #[async_trait]
    impl CallApp for WaitForeverApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Custom
        }
        fn name(&self) -> &str {
            "wait-forever"
        }

        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            ctrl.answer().await?;
            Ok(AppAction::Continue)
        }
    }

    #[tokio::test]
    async fn test_remote_hangup_exits_loop() {
        let mut stack = MockCallStack::run(Box::new(WaitForeverApp), "1001", "9000");
        stack
            .assert_cmd(100, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // Remote party hangs up — loop should exit cleanly
        stack.remote_hangup();
        stack
            .join()
            .await
            .expect("loop should exit without error after remote hangup");
    }

    // ── 4. AppAction::Chain ───────────────────────────────────────────────────

    /// First app in chain — plays a greeting then chains to SecondApp.
    struct FirstApp;

    #[async_trait]
    impl CallApp for FirstApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Ivr
        }
        fn name(&self) -> &str {
            "first"
        }

        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            ctrl.answer().await?;
            Ok(AppAction::Continue)
        }

        async fn on_audio_complete(
            &mut self,
            _id: String,
            _ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            Ok(AppAction::Chain(Box::new(SecondApp)))
        }
    }

    /// Second app — just hangs up immediately on enter.
    struct SecondApp;

    #[async_trait]
    impl CallApp for SecondApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Voicemail
        }
        fn name(&self) -> &str {
            "second"
        }

        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            ctrl.hangup(None).await?;
            Ok(AppAction::Exit)
        }
    }

    #[tokio::test]
    async fn test_chain() {
        let mut stack = MockCallStack::run(Box::new(FirstApp), "1001", "1002");

        // First app answers
        stack
            .assert_cmd(100, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // Trigger chain
        stack.audio_complete("default");

        // Second app hangs up
        stack
            .assert_cmd(100, "Hangup", |c| matches!(c, SessionAction::Hangup { .. }))
            .await;
    }

    // ── 5. Named timers: set_timeout + on_timeout ─────────────────────────────

    struct TimerApp {
        log: EventLog,
        fired_count: usize,
    }

    #[async_trait]
    impl CallApp for TimerApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Queue
        }
        fn name(&self) -> &str {
            "timer"
        }

        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            ctrl.answer().await?;
            ctrl.set_timeout("tick", Duration::from_millis(20));
            self.log.lock().unwrap().push("enter".into());
            Ok(AppAction::Continue)
        }

        async fn on_timeout(
            &mut self,
            id: String,
            _ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            self.log.lock().unwrap().push(format!("timeout:{id}"));
            self.fired_count += 1;
            if self.fired_count >= 1 {
                Ok(AppAction::Hangup { reason: None })
            } else {
                Ok(AppAction::Continue)
            }
        }
    }

    #[tokio::test]
    async fn test_set_timeout_fires_on_timeout() {
        let log = new_log();
        let app = TimerApp {
            log: log.clone(),
            fired_count: 0,
        };
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2001");

        // App answers on enter
        stack
            .assert_cmd(100, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // Timer fires after 20ms → on_timeout → Hangup
        stack
            .assert_cmd(200, "Hangup from timer", |c| {
                matches!(c, SessionAction::Hangup { .. })
            })
            .await;

        assert_eq!(logged(&log), vec!["enter", "timeout:tick"]);
    }

    // ── 6. cancel_timeout suppresses fire ────────────────────────────────────

    struct CancelTimerApp {
        log: EventLog,
    }

    #[async_trait]
    impl CallApp for CancelTimerApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Custom
        }
        fn name(&self) -> &str {
            "cancel-timer"
        }

        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            ctrl.answer().await?;
            ctrl.set_timeout("never", Duration::from_millis(20));
            ctrl.cancel_timeout("never"); // immediately suppress
            self.log.lock().unwrap().push("enter".into());
            Ok(AppAction::Continue)
        }

        async fn on_timeout(
            &mut self,
            id: String,
            _ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            self.log.lock().unwrap().push(format!("timeout:{id}"));
            Ok(AppAction::Hangup { reason: None })
        }
    }

    #[tokio::test]
    async fn test_cancel_timeout_suppresses_fire() {
        let log = new_log();
        let app = CancelTimerApp { log: log.clone() };
        let mut stack = MockCallStack::run(Box::new(app), "1001", "2002");

        stack
            .assert_cmd(100, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // Wait 60ms — cancelled timer must NOT fire
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            stack.drain_cmds().is_empty(),
            "cancelled timer must not produce a command"
        );
        assert!(
            !logged(&log).iter().any(|e| e.starts_with("timeout:")),
            "on_timeout must not be called"
        );

        stack.cancel();
        let _ = stack.join().await;
    }

    // ── 7. inter_digit_timeout in collect_dtmf ────────────────────────────────

    /// App that uses collect_dtmf with a 30ms inter-digit gap.
    struct CollectApp {
        log: EventLog,
    }

    #[async_trait]
    impl CallApp for CollectApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Ivr
        }
        fn name(&self) -> &str {
            "collect"
        }

        async fn on_enter(
            &mut self,
            ctrl: &mut CallController,
            _ctx: &ApplicationContext,
        ) -> Result<AppAction> {
            ctrl.answer().await?;
            let digits = ctrl
                .collect_dtmf(DtmfCollectConfig {
                    min_digits: 1,
                    max_digits: 4,
                    timeout: Duration::from_millis(500),
                    terminator: None,
                    play_prompt: None,
                    inter_digit_timeout: Some(Duration::from_millis(40)),
                })
                .await?;
            self.log.lock().unwrap().push(format!("collected:{digits}"));
            Ok(AppAction::Hangup { reason: None })
        }
    }

    #[tokio::test]
    async fn test_collect_dtmf_inter_digit_timeout() {
        let log = new_log();
        let app = CollectApp { log: log.clone() };
        let mut stack = MockCallStack::run(Box::new(app), "1001", "3001");

        // AcceptCall first
        stack
            .assert_cmd(200, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        // Send two digits quickly
        stack.dtmf("4");
        tokio::time::sleep(Duration::from_millis(10)).await;
        stack.dtmf("2");

        // Pause longer than inter_digit_timeout (40ms) → collection completes
        tokio::time::sleep(Duration::from_millis(60)).await;

        // App should now hang up
        stack
            .assert_cmd(200, "Hangup", |c| matches!(c, SessionAction::Hangup { .. }))
            .await;

        assert!(logged(&log).contains(&"collected:42".to_string()));
    }

    // ── 8. System cancellation ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_cancel_exits_cleanly() {
        let mut stack = MockCallStack::run(Box::new(WaitForeverApp), "1001", "9999");
        stack
            .assert_cmd(100, "AcceptCall", |c| {
                matches!(c, SessionAction::AcceptCall { .. })
            })
            .await;

        stack.cancel();
        stack
            .join()
            .await
            .expect("cancelled loop should exit without error");
    }
}
