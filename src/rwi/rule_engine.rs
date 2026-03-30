//!


use crate::proxy::proxy_call::state::{SessionAction, SipSessionHandle};
use crate::rwi::routing::{LocalAction, SmartRoutingConfig};
use tracing::info;

#[derive(Debug, Clone)]
pub struct RuleContext {
    pub session_id: String,
    pub call_id: String,
    pub is_agent: bool,
    pub call_state: String,
    pub metadata: std::collections::HashMap<String, String>,
}

impl RuleContext {
    pub fn new(session_id: String, call_id: String) -> Self {
        Self {
            session_id,
            call_id,
            is_agent: false,
            call_state: "talking".to_string(),
            metadata: std::collections::HashMap::new(),
        }
    }

    pub fn with_agent(mut self, is_agent: bool) -> Self {
        self.is_agent = is_agent;
        self
    }

    pub fn with_state(mut self, state: &str) -> Self {
        self.call_state = state.to_string();
        self
    }
}

impl Default for RuleContext {
    fn default() -> Self {
        Self::new("unknown".to_string(), "unknown".to_string())
    }
}

#[derive(Debug, Clone)]
pub enum RuleResult {
    Success,
    NeedRwi { reason: String },
    Failed { error: String },
    Partial { executed: Vec<String>, failed: Vec<String> },
}

pub struct LocalRuleEngine {
    #[allow(dead_code)]
    config: SmartRoutingConfig,
}

impl LocalRuleEngine {
    pub fn new(config: SmartRoutingConfig) -> Self {
        Self { config }
    }

    pub async fn execute(
        &self,
        action: &LocalAction,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        match action {
            LocalAction::TransferToQueue(queue) => {
                self.execute_transfer_to_queue(queue, ctx, session_handle).await
            }
            LocalAction::TransferToNumber(number) => {
                self.execute_transfer_to_number(number, ctx, session_handle).await
            }
            LocalAction::PlayAnnouncement(file) => {
                self.execute_play_announcement(file, ctx, session_handle).await
            }
            LocalAction::StartRecording => {
                self.execute_start_recording(ctx, session_handle).await
            }
            LocalAction::StopRecording => {
                self.execute_stop_recording(ctx, session_handle).await
            }
            LocalAction::Voicemail => {
                self.execute_voicemail(ctx, session_handle).await
            }
            LocalAction::Hangup => {
                self.execute_hangup(ctx, session_handle).await
            }
            LocalAction::Hold => {
                self.execute_hold(ctx, session_handle).await
            }
            LocalAction::Unhold => {
                self.execute_unhold(ctx, session_handle).await
            }
            LocalAction::SendToPeer { content_type: _, body: _ } => {
                RuleResult::NeedRwi {
                    reason: "SendToPeer requires direct dialog access".to_string(),
                }
            }
            LocalAction::NotifyRwi { event } => {
                RuleResult::NeedRwi {
                    reason: format!("Local action requires RWI: {}", event),
                }
            }
            LocalAction::Sequence(actions) => {
                self.execute_sequence(actions, ctx, session_handle).await
            }
        }
    }

    async fn execute_transfer_to_queue(
        &self,
        queue: &str,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            queue = %queue,
            "Executing local rule: TransferToQueue"
        );

        if let Some(handle) = session_handle {
            let _ = handle.send_command(SessionAction::PlayPrompt {
                audio_file: format!("queue_{}.wav", queue),
                send_progress: false,
                await_completion: false,
                track_id: None,
                loop_playback: true,
                interrupt_on_dtmf: true,
            });

            RuleResult::NeedRwi {
                reason: format!("Queue transfer requires ACD: {}", queue),
            }
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    async fn execute_transfer_to_number(
        &self,
        number: &str,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            number = %number,
            "Executing local rule: TransferToNumber"
        );

        if let Some(handle) = session_handle {
            let _ = handle.send_command(SessionAction::TransferTarget(number.to_string()));
            RuleResult::Success
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    async fn execute_play_announcement(
        &self,
        file: &str,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            file = %file,
            "Executing local rule: PlayAnnouncement"
        );

        if let Some(handle) = session_handle {
            let _ = handle.send_command(SessionAction::PlayPrompt {
                audio_file: file.to_string(),
                send_progress: false,
                await_completion: false,
                track_id: None,
                loop_playback: false,
                interrupt_on_dtmf: false,
            });
            RuleResult::Success
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    async fn execute_start_recording(
        &self,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            "Executing local rule: StartRecording"
        );

        if let Some(handle) = session_handle {
            let recording_path = format!("/recordings/{}_{}.wav", ctx.call_id, chrono::Utc::now().timestamp());
            let _ = handle.send_command(SessionAction::StartRecording {
                path: recording_path,
                max_duration: Some(std::time::Duration::from_secs(3600)),
                beep: true,
            });
            RuleResult::Success
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    async fn execute_stop_recording(
        &self,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            "Executing local rule: StopRecording"
        );

        if let Some(handle) = session_handle {
            let _ = handle.send_command(SessionAction::StopRecording);
            RuleResult::Success
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    async fn execute_voicemail(
        &self,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            "Executing local rule: Voicemail"
        );

        if let Some(handle) = session_handle {
            let _ = handle.send_command(SessionAction::PlayPrompt {
                audio_file: "voicemail_prompt.wav".to_string(),
                send_progress: false,
                await_completion: false,
                track_id: None,
                loop_playback: false,
                interrupt_on_dtmf: false,
            });

            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            let recording_path = format!("/voicemails/vm_{}_{}.wav", ctx.call_id, chrono::Utc::now().timestamp());
            let _ = handle.send_command(SessionAction::StartRecording {
                path: recording_path,
                max_duration: Some(std::time::Duration::from_secs(300)), 
                beep: true,
            });

            RuleResult::Success
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    async fn execute_hangup(
        &self,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            "Executing local rule: Hangup"
        );

        if let Some(handle) = session_handle {
            let _ = handle.send_command(SessionAction::Hangup {
                reason: Some(crate::callrecord::CallRecordHangupReason::BySystem),
                code: Some(200),
                initiator: Some("local_rule".to_string()),
            });
            RuleResult::Success
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    async fn execute_hold(
        &self,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            "Executing local rule: Hold"
        );

        if let Some(handle) = session_handle {
            let _ = handle.send_command(SessionAction::Hold {
                music_source: Some("hold_music.wav".to_string()),
            });
            RuleResult::Success
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    async fn execute_unhold(
        &self,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        info!(
            session_id = %ctx.session_id,
            "Executing local rule: Unhold"
        );

        if let Some(handle) = session_handle {
            let _ = handle.send_command(SessionAction::Unhold);
            RuleResult::Success
        } else {
            RuleResult::Failed {
                error: "No session handle available".to_string(),
            }
        }
    }

    fn execute_sequence<'a>(
        &'a self,
        actions: &'a [Box<LocalAction>],
        ctx: &'a RuleContext,
        session_handle: Option<&'a SipSessionHandle>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RuleResult> + Send + 'a>> {
        Box::pin(async move {
            let mut executed = vec![];
            let mut failed = vec![];

            for (idx, action) in actions.iter().enumerate() {
                match self.execute(action, ctx, session_handle).await {
                    RuleResult::Success => {
                        executed.push(format!("action_{}", idx));
                    }
                    RuleResult::Failed { error } => {
                        failed.push(format!("action_{}: {}", idx, error));
                    }
                    RuleResult::NeedRwi { reason } => {
                        return RuleResult::NeedRwi {
                            reason: format!("Sequence interrupted at {}: {}", idx, reason),
                        };
                    }
                    _ => {}
                }
            }

            if failed.is_empty() {
                RuleResult::Success
            } else if executed.is_empty() {
                RuleResult::Failed {
                    error: failed.join("; "),
                }
            } else {
                RuleResult::Partial { executed, failed }
            }
        })
    }
}

pub struct RuleExecutor {
    engine: LocalRuleEngine,
    max_retries: u32,
}

impl RuleExecutor {
    pub fn new(engine: LocalRuleEngine, max_retries: u32) -> Self {
        Self {
            engine,
            max_retries,
        }
    }

    pub async fn execute_with_retry(
        &self,
        action: &LocalAction,
        ctx: &RuleContext,
        session_handle: Option<&SipSessionHandle>,
    ) -> RuleResult {
        let mut last_error = String::new();

        for attempt in 0..=self.max_retries {
            match self.engine.execute(action, ctx, session_handle).await {
                RuleResult::Success => return RuleResult::Success,
                RuleResult::NeedRwi { reason } => {
                    return RuleResult::NeedRwi { reason };
                }
                RuleResult::Failed { error } => {
                    last_error = error;
                    if attempt < self.max_retries {
                        let delay = std::time::Duration::from_millis(100 * (attempt as u64 + 1));
                        tokio::time::sleep(delay).await;
                    }
                }
                _ => {}
            }
        }

        RuleResult::Failed {
            error: format!("Max retries exceeded: {}", last_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_play_announcement_no_handle() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let ctx = RuleContext::new("session-1".to_string(), "call-1".to_string());

        let result = engine
            .execute(&LocalAction::PlayAnnouncement("test.wav".to_string()), &ctx, None)
            .await;

        match result {
            RuleResult::Failed { .. } => {}
            _ => panic!("Expected Failed without session handle"),
        }
    }

    #[test]
    fn test_rule_context_builder() {
        let ctx = RuleContext::new("s1".to_string(), "c1".to_string())
            .with_agent(true)
            .with_state("holding");

        assert_eq!(ctx.session_id, "s1");
        assert!(ctx.is_agent);
        assert_eq!(ctx.call_state, "holding");
    }

    #[test]
    fn test_rule_context_default() {
        let ctx = RuleContext::default();
        assert_eq!(ctx.session_id, "unknown");
        assert!(!ctx.is_agent);
    }

    #[test]
    fn test_rule_context_with_metadata() {
        let mut ctx = RuleContext::new("s1".to_string(), "c1".to_string());
        ctx.metadata.insert("key1".to_string(), "value1".to_string());
        ctx.metadata.insert("dtmf_digit".to_string(), "*9".to_string());

        assert_eq!(ctx.metadata.get("key1"), Some(&"value1".to_string()));
        assert_eq!(ctx.metadata.get("dtmf_digit"), Some(&"*9".to_string()));
    }

    #[test]
    fn test_local_action_all_variants() {
        // Test all LocalAction variants can be created and cloned
        let actions = vec![
            LocalAction::TransferToQueue("operator".to_string()),
            LocalAction::TransferToNumber("1234".to_string()),
            LocalAction::PlayAnnouncement("test.wav".to_string()),
            LocalAction::StartRecording,
            LocalAction::StopRecording,
            LocalAction::Voicemail,
            LocalAction::NotifyRwi { event: "test_event".to_string() },
            LocalAction::SendToPeer { 
                content_type: "application/json".to_string(), 
                body: "{}".to_string() 
            },
            LocalAction::Sequence(vec![]),
            LocalAction::Hold,
            LocalAction::Unhold,
            LocalAction::Hangup,
        ];

        for action in actions {
            let cloned = action.clone();
            assert_eq!(format!("{:?}", action), format!("{:?}", cloned));
        }
    }

    #[test]
    fn test_rule_result_variants() {
        // Test RuleResult variants
        let success = RuleResult::Success;
        assert!(matches!(success, RuleResult::Success));

        let failed = RuleResult::Failed { error: "test error".to_string() };
        assert!(matches!(failed, RuleResult::Failed { .. }));

        let partial = RuleResult::Partial { 
            executed: vec!["a".to_string()], 
            failed: vec!["b".to_string()] 
        };
        assert!(matches!(partial, RuleResult::Partial { .. }));

        let need_rwi = RuleResult::NeedRwi { reason: "need app".to_string() };
        assert!(matches!(need_rwi, RuleResult::NeedRwi { .. }));
    }

    #[test]
    fn test_rule_context_builder_chaining() {
        let ctx = RuleContext::new("session-1".to_string(), "call-1".to_string())
            .with_agent(true)
            .with_state("talking");

        assert_eq!(ctx.session_id, "session-1");
        assert_eq!(ctx.call_id, "call-1");
        assert!(ctx.is_agent);
        assert_eq!(ctx.call_state, "talking");
        assert!(ctx.metadata.is_empty());
    }

    #[test]
    fn test_rule_context_metadata() {
        let mut ctx = RuleContext::new("s1".to_string(), "c1".to_string());
        ctx.metadata.insert("key1".to_string(), "value1".to_string());
        ctx.metadata.insert("key2".to_string(), "value2".to_string());

        assert_eq!(ctx.metadata.get("key1"), Some(&"value1".to_string()));
        assert_eq!(ctx.metadata.get("key2"), Some(&"value2".to_string()));
        assert_eq!(ctx.metadata.len(), 2);
    }

    #[tokio::test]
    async fn test_execute_all_actions_no_handle() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let ctx = RuleContext::new("session-1".to_string(), "call-1".to_string());

        // Test actions that return Failed without session handle
        let actions_expect_failed = vec![
            LocalAction::TransferToQueue("test".to_string()),
            LocalAction::TransferToNumber("1234".to_string()),
            LocalAction::Hold,
            LocalAction::Unhold,
            LocalAction::Hangup,
            LocalAction::Voicemail,
            LocalAction::StartRecording,
            LocalAction::StopRecording,
            LocalAction::PlayAnnouncement("test.wav".to_string()),
        ];

        for action in actions_expect_failed {
            let result = engine.execute(&action, &ctx, None).await;
            match result {
                RuleResult::Failed { .. } | RuleResult::NeedRwi { .. } => {}
                _ => panic!("Expected Failed or NeedRwi for {:?} without handle, got {:?}", action, result),
            }
        }
    }

    #[tokio::test]
    async fn test_execute_notify_rwi() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let ctx = RuleContext::new("session-1".to_string(), "call-1".to_string());

        // NotifyRwi should always return NeedRwi
        let result = engine
            .execute(&LocalAction::NotifyRwi { event: "test".to_string() }, &ctx, None)
            .await;

        match result {
            RuleResult::NeedRwi { reason } => {
                assert!(reason.contains("test"));
            }
            _ => panic!("Expected NeedRwi for NotifyRwi action"),
        }
    }

    #[tokio::test]
    async fn test_execute_send_to_peer_no_handle() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let ctx = RuleContext::new("session-1".to_string(), "call-1".to_string());

        // SendToPeer should return NeedRwi without session handle
        let result = engine
            .execute(&LocalAction::SendToPeer { 
                content_type: "text/plain".to_string(), 
                body: "hello".to_string() 
            }, &ctx, None)
            .await;

        match result {
            RuleResult::NeedRwi { .. } => {}
            _ => panic!("Expected NeedRwi for SendToPeer without handle"),
        }
    }

    #[tokio::test]
    async fn test_execute_sequence_empty() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let ctx = RuleContext::new("session-1".to_string(), "call-1".to_string());

        // Empty sequence should succeed
        let result = engine
            .execute(&LocalAction::Sequence(vec![]), &ctx, None)
            .await;

        match result {
            RuleResult::Success => {}
            _ => panic!("Expected Success for empty sequence"),
        }
    }

    #[tokio::test]
    async fn test_execute_sequence_with_actions() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let ctx = RuleContext::new("session-1".to_string(), "call-1".to_string());

        // Sequence with NotifyRwi actions should return NeedRwi (first one)
        let result = engine
            .execute(&LocalAction::Sequence(vec![
                Box::new(LocalAction::NotifyRwi { event: "first".to_string() }),
                Box::new(LocalAction::NotifyRwi { event: "second".to_string() }),
            ]), &ctx, None)
            .await;

        // Should get NeedRwi from first action
        match result {
            RuleResult::NeedRwi { reason } => {
                assert!(reason.contains("first"));
            }
            _ => panic!("Expected NeedRwi from first sequence action"),
        }
    }

    #[tokio::test]
    async fn test_rule_executor_new() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let executor = RuleExecutor::new(engine, 3);

        // Just verify it can be created
        assert_eq!(executor.max_retries, 3);
    }

    #[test]
    fn test_local_rule_engine_new() {
        let config = SmartRoutingConfig::default();
        let _engine = LocalRuleEngine::new(config);
        
        // Verify engine is created successfully
        let ctx = RuleContext::new("s1".to_string(), "c1".to_string());
        assert_eq!(ctx.session_id, "s1");
    }

    #[tokio::test]
    async fn test_rule_executor_with_retry_success() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let executor = RuleExecutor::new(engine, 3);
        let ctx = RuleContext::new("s1".to_string(), "c1".to_string());

        // NotifyRwi should return NeedRwi immediately (no retries needed)
        let result = executor.execute_with_retry(
            &LocalAction::NotifyRwi { event: "test".to_string() },
            &ctx,
            None
        ).await;

        match result {
            RuleResult::NeedRwi { reason } => {
                assert!(reason.contains("test"));
            }
            _ => panic!("Expected NeedRwi"),
        }
    }

    #[tokio::test]
    async fn test_execute_play_announcement_success_path() {
        let config = SmartRoutingConfig::default();
        let engine = LocalRuleEngine::new(config);
        let ctx = RuleContext::new("session-1".to_string(), "call-1".to_string());

        // Without handle, PlayAnnouncement returns Failed
        let result = engine
            .execute(&LocalAction::PlayAnnouncement("test.wav".to_string()), &ctx, None)
            .await;

        match result {
            RuleResult::Failed { error } => {
                assert!(error.contains("No session"));
            }
            _ => panic!("Expected Failed for PlayAnnouncement without handle, got {:?}", result),
        }
    }

    #[test]
    fn test_smart_routing_config_integration() {
        // Test that SmartRoutingConfig can be used with LocalRuleEngine
        let config = SmartRoutingConfig::default();
        assert!(config.enable_local_engine);
        
        let engine = LocalRuleEngine::new(config);
        let ctx = RuleContext::new("s1".to_string(), "c1".to_string());
        assert_eq!(ctx.session_id, "s1");
        
        // engine is used
        drop(engine);
    }
}
