use crate::call::app::{AppAction, AppEvent, CallApp, CallAppType, ApplicationContext};
use crate::call::app::{CallController, ExitReason};
use crate::call::RouteContext;
use crate::rwi::gateway::{RwiGateway, SessionId};
use crate::rwi::proto::RwiEvent;
use crate::rwi::session::OwnershipMode;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

pub const RWI_APP_NAME: &str = "rwi";

#[derive(Clone)]
pub struct RwiAddon {
    gateway: Arc<RwLock<RwiGateway>>,
}

impl RwiAddon {
    pub fn new(gateway: Arc<RwLock<RwiGateway>>) -> Self {
        Self { gateway }
    }
}

#[async_trait]
impl crate::call::CallAppFactory for RwiAddon {
    async fn create_app(
        &self,
        app_name: &str,
        _context: &RouteContext<'_>,
        params: &serde_json::Value,
    ) -> Option<Box<dyn CallApp>> {
        if app_name != RWI_APP_NAME {
            return None;
        }

        let context_name = params
            .get("context")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();

        let session_id = params
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        Some(Box::new(RwiApp::new(context_name, session_id, self.gateway.clone())))
    }
}

pub struct RwiApp {
    context_name: String,
    session_id: Option<SessionId>,
    gateway: Arc<RwLock<RwiGateway>>,
    owned: bool,
    /// The call_id of the call this app is handling, set in `on_enter`.
    owned_call_id: Option<String>,
    /// Track ID of the currently playing audio, if any.
    current_track_id: Option<String>,
    /// If `true`, the next DTMF digit will interrupt the current playback.
    interrupt_on_dtmf: bool,
}

impl RwiApp {
    pub fn new(context_name: String, session_id: Option<SessionId>, gateway: Arc<RwLock<RwiGateway>>) -> Self {
        Self {
            context_name,
            session_id,
            gateway,
            owned: false,
            owned_call_id: None,
            current_track_id: None,
            interrupt_on_dtmf: false,
        }
    }

    async fn send_event(&self, event: RwiEvent) {
        let gw = self.gateway.read().await;
        if let Some(session_id) = &self.session_id {
            gw.send_event_to_session(session_id, &event);
        }
        gw.fan_out_event_to_context(&self.context_name, &event);
    }
}

#[async_trait]
impl CallApp for RwiApp {
    fn app_type(&self) -> CallAppType {
        CallAppType::Custom
    }

    fn name(&self) -> &str {
        RWI_APP_NAME
    }

    async fn on_enter(
        &mut self,
        _controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let call_id = context.call_info.session_id.clone();

        self.send_event(RwiEvent::CallIncoming(crate::rwi::proto::CallIncomingData {
            call_id: call_id.clone(),
            context: self.context_name.clone(),
            caller: context.call_info.caller.clone(),
            callee: context.call_info.callee.clone(),
            direction: context.call_info.direction.clone(),
            trunk: None,
            sip_headers: std::collections::HashMap::new(),
        })).await;

        if let Some(session_id) = &self.session_id {
            let claim_ok = {
                let mut gateway = self.gateway.write().await;
                gateway
                    .claim_call_ownership(session_id, call_id.clone(), OwnershipMode::Control)
                    .await
                    .is_ok()
            };

            if claim_ok {
                self.owned = true;
                self.owned_call_id = Some(call_id.clone());
                self.send_event(RwiEvent::CallAnswered { call_id: call_id.clone() }).await;
            }
        }

        Ok(AppAction::Continue)
    }

    async fn on_dtmf(
        &mut self,
        digit: String,
        controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if self.interrupt_on_dtmf {
            if let Some(track_id) = self.current_track_id.take() {
                controller.stop_audio().await.ok();
                self.interrupt_on_dtmf = false;
                self.send_event(RwiEvent::MediaPlayFinished {
                    call_id: context.call_info.session_id.clone(),
                    track_id,
                    interrupted: true,
                }).await;
            }
        }

        self.send_event(RwiEvent::Dtmf {
            call_id: context.call_info.session_id.clone(),
            digit,
        }).await;
        Ok(AppAction::Continue)
    }

    async fn on_audio_complete(
        &mut self,
        track_id: String,
        _controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        if self.current_track_id.as_deref() == Some(&track_id) {
            self.current_track_id = None;
            self.interrupt_on_dtmf = false;
        }
        self.send_event(RwiEvent::MediaPlayFinished {
            call_id: context.call_info.session_id.clone(),
            track_id,
            interrupted: false,
        }).await;
        Ok(AppAction::Continue)
    }

    async fn on_external_event(
        &mut self,
        event: AppEvent,
        controller: &mut CallController,
        _context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        match event {
            AppEvent::Custom { ref name, ref data } if name == "media.play" => {
                let audio_file = data
                    .get("audio_file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let track_id = data
                    .get("track_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default")
                    .to_string();
                let interrupt_on_dtmf = data
                    .get("interrupt_on_dtmf")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let loop_playback = data
                    .get("loop")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if !audio_file.is_empty() {
                    match controller.play_audio_with_options(
                        &audio_file,
                        Some(track_id.clone()),
                        loop_playback,
                        interrupt_on_dtmf,
                    ).await {
                        Ok(_handle) => {
                            self.current_track_id = Some(track_id);
                            self.interrupt_on_dtmf = interrupt_on_dtmf;
                        }
                        Err(e) => {
                            tracing::warn!(
                                track_id = %track_id,
                                error = %e,
                                "RwiApp: media.play failed"
                            );
                        }
                    }
                }
            }
            AppEvent::Custom { name, data } => {
                tracing::debug!(event = %name, data = ?data, "RwiApp custom event");
            }
            _ => {}
        }
        Ok(AppAction::Continue)
    }

    async fn on_timeout(
        &mut self,
        timeout_id: String,
        _controller: &mut CallController,
        _context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        tracing::debug!(timeout_id = %timeout_id, "RwiApp timeout");
        Ok(AppAction::Continue)
    }

    async fn on_exit(&mut self, reason: ExitReason) -> anyhow::Result<()> {
        let _ = reason;
        if let (Some(session_id), Some(call_id)) = (&self.session_id, &self.owned_call_id) {
            let mut gateway = self.gateway.write().await;
            if self.owned {
                gateway.release_call_ownership(session_id, call_id).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rwi::auth::RwiIdentity;

    fn create_test_gateway() -> Arc<RwLock<RwiGateway>> {
        Arc::new(RwLock::new(RwiGateway::new()))
    }

    #[test]
    fn test_rwi_addon_new() {
        let gateway = create_test_gateway();
        let addon = RwiAddon::new(gateway);
        let _ = addon;
    }

    #[test]
    fn test_rwi_app_name() {
        let gateway = create_test_gateway();
        let app = RwiApp::new("test_ctx".to_string(), None, gateway);
        assert_eq!(app.name(), "rwi");
        assert_eq!(app.app_type(), CallAppType::Custom);
    }

    #[tokio::test]
    async fn test_gateway_claim_ownership() {
        let gateway = create_test_gateway();

        let identity = RwiIdentity {
            token: "test-token".to_string(),
            scopes: vec!["call.control".to_string()],
        };

        let mut gw = gateway.write().await;
        let session = gw.create_session(identity);
        let session_id = session.read().await.id.clone();

        let call_id = "test-call-001".to_string();
        let result = gw.claim_call_ownership(&session_id, call_id.clone(), OwnershipMode::Control).await;
        assert!(result.is_ok());

        assert_eq!(gw.get_call_owner(&call_id), Some(session_id));
    }

    #[tokio::test]
    async fn test_gateway_claim_already_owned() {
        let gateway = create_test_gateway();

        let identity1 = RwiIdentity {
            token: "token1".to_string(),
            scopes: vec!["call.control".to_string()],
        };
        let identity2 = RwiIdentity {
            token: "token2".to_string(),
            scopes: vec!["call.control".to_string()],
        };

        let mut gw = gateway.write().await;
        let session1 = gw.create_session(identity1);
        let session1_id = session1.read().await.id.clone();
        let session2 = gw.create_session(identity2);
        let session2_id = session2.read().await.id.clone();

        let call_id = "test-call-001".to_string();
        let result1 = gw.claim_call_ownership(&session1_id, call_id.clone(), OwnershipMode::Control).await;
        assert!(result1.is_ok());

        let result2 = gw.claim_call_ownership(&session2_id, call_id.clone(), OwnershipMode::Control).await;
        assert!(result2.is_err());
    }

    #[tokio::test]
    async fn test_gateway_release_ownership() {
        let gateway = create_test_gateway();

        let identity = RwiIdentity {
            token: "test-token".to_string(),
            scopes: vec!["call.control".to_string()],
        };

        let mut gw = gateway.write().await;
        let session = gw.create_session(identity);
        let session_id = session.read().await.id.clone();

        let call_id = "test-call-001".to_string();
        gw.claim_call_ownership(&session_id, call_id.clone(), OwnershipMode::Control).await.unwrap();

        assert_eq!(gw.get_call_owner(&call_id), Some(session_id.clone()));

        gw.release_call_ownership(&session_id, &call_id).await;
        assert!(gw.get_call_owner(&call_id).is_none());
    }

    #[tokio::test]
    async fn test_gateway_session_subscribe() {
        let gateway = create_test_gateway();

        let identity = RwiIdentity {
            token: "test-token".to_string(),
            scopes: vec!["call.control".to_string()],
        };

        let mut gw = gateway.write().await;
        let session = gw.create_session(identity);
        let session_id = session.read().await.id.clone();

        gw.subscribe(&session_id, vec!["context1".to_string(), "context2".to_string()]).await;

        let subscribers = gw.get_sessions_subscribed_to_context("context1");
        assert!(subscribers.contains(&session_id));

        let subscribers2 = gw.get_sessions_subscribed_to_context("context2");
        assert!(subscribers2.contains(&session_id));

        let subscribers3 = gw.get_sessions_subscribed_to_context("context3");
        assert!(subscribers3.is_empty());
    }

    // ── send_event delivers to owning session ──────────────────────────────

    #[tokio::test]
    async fn test_send_event_reaches_owning_session() {
        let gateway = create_test_gateway();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        let session_id = {
            let mut gw = gateway.write().await;
            let identity = RwiIdentity {
                token: "tok".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            sid
        };

        let app = RwiApp::new("ctx".to_string(), Some(session_id.clone()), gateway.clone());
        app.send_event(crate::rwi::proto::RwiEvent::Dtmf {
            call_id: "c1".to_string(),
            digit: "5".to_string(),
        })
        .await;

        let v = event_rx.try_recv().expect("event should be delivered");
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains("dtmf") || s.contains("Dtmf"), "event should be a DTMF event: {s}");
        assert!(s.contains("\"5\"") || s.contains("5"), "event should contain digit 5: {s}");
    }

    #[tokio::test]
    async fn test_send_event_reaches_context_subscriber() {
        let gateway = create_test_gateway();
        let (sub_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel();

        {
            let mut gw = gateway.write().await;
            let identity = RwiIdentity {
                token: "sub-tok".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, sub_tx);
            gw.subscribe(&sid, vec!["my-ctx".to_string()]).await;
        }

        let app = RwiApp::new("my-ctx".to_string(), None, gateway.clone());
        app.send_event(crate::rwi::proto::RwiEvent::Dtmf {
            call_id: "c2".to_string(),
            digit: "9".to_string(),
        })
        .await;

        let v = sub_rx.try_recv().expect("subscriber should receive event");
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains("c2"), "event should reference call c2: {s}");
    }

    #[tokio::test]
    async fn test_send_event_no_owner_no_subscriber_does_not_panic() {
        let gateway = create_test_gateway();
        let app = RwiApp::new("empty-ctx".to_string(), None, gateway.clone());
        app.send_event(crate::rwi::proto::RwiEvent::Dtmf {
            call_id: "c3".to_string(),
            digit: "1".to_string(),
        })
        .await;
    }

    #[tokio::test]
    async fn test_send_event_owner_and_subscriber_both_receive() {
        let gateway = create_test_gateway();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let session_id = {
            let mut gw = gateway.write().await;
            let identity = RwiIdentity { token: "dual".into(), scopes: vec![] };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, tx);
            gw.subscribe(&sid, vec!["dual-ctx".to_string()]).await;
            sid
        };

        let app = RwiApp::new("dual-ctx".to_string(), Some(session_id), gateway.clone());
        app.send_event(crate::rwi::proto::RwiEvent::Dtmf {
            call_id: "c4".to_string(),
            digit: "0".to_string(),
        })
        .await;

        assert!(
            rx.try_recv().is_ok(),
            "at least one event delivery expected"
        );
    }

    #[tokio::test]
    async fn test_on_exit_releases_correct_call_id() {
        let gateway = create_test_gateway();

        let session_id = {
            let mut gw = gateway.write().await;
            let identity = RwiIdentity { token: "tok".into(), scopes: vec![] };
            let session = gw.create_session(identity);
            session.read().await.id.clone()
        };

        let call_id = "call-xyz".to_string();
        {
            let mut gw = gateway.write().await;
            gw.claim_call_ownership(&session_id, call_id.clone(), OwnershipMode::Control)
                .await
                .expect("claim should succeed");
        }
        assert_eq!(
            gateway.read().await.get_call_owner(&call_id),
            Some(session_id.clone()),
            "call should be owned before exit"
        );

        let mut app = RwiApp {
            context_name: "ctx".to_string(),
            session_id: Some(session_id.clone()),
            gateway: gateway.clone(),
            owned: true,
            owned_call_id: Some(call_id.clone()),
            current_track_id: None,
            interrupt_on_dtmf: false,
        };

        app.on_exit(ExitReason::Normal).await.expect("on_exit should not error");

        assert_eq!(
            gateway.read().await.get_call_owner(&call_id),
            None,
            "call ownership should be released after on_exit"
        );
    }

    #[tokio::test]
    async fn test_on_exit_does_not_release_when_not_owned() {
        let gateway = create_test_gateway();

        let session_a = {
            let mut gw = gateway.write().await;
            let id = RwiIdentity { token: "a".into(), scopes: vec![] };
            let s = gw.create_session(id);
            s.read().await.id.clone()
        };
        let call_id = "call-abc".to_string();
        {
            let mut gw = gateway.write().await;
            gw.claim_call_ownership(&session_a, call_id.clone(), OwnershipMode::Control)
                .await
                .expect("claim should succeed");
        }

        let session_b = {
            let mut gw = gateway.write().await;
            let id = RwiIdentity { token: "b".into(), scopes: vec![] };
            let s = gw.create_session(id);
            s.read().await.id.clone()
        };
        let mut app = RwiApp {
            context_name: "ctx".to_string(),
            session_id: Some(session_b),
            gateway: gateway.clone(),
            owned: false,
            owned_call_id: Some(call_id.clone()),
            current_track_id: None,
            interrupt_on_dtmf: false,
        };

        app.on_exit(ExitReason::Normal).await.expect("on_exit should not error");

        assert_eq!(
            gateway.read().await.get_call_owner(&call_id),
            Some(session_a),
            "ownership must not be released by a non-owning app"
        );
    }

    /// on_exit when owned_call_id is None (no call was ever claimed) must not panic.
    #[tokio::test]
    async fn test_on_exit_no_call_id_does_not_panic() {
        let gateway = create_test_gateway();
        let mut app = RwiApp::new("ctx".to_string(), None, gateway.clone());
        app.on_exit(ExitReason::Normal).await.expect("on_exit should not error");
    }

    // ── on_enter event ordering tests ─────────────────────────────────────

    /// Verifies that CallIncoming arrives before CallAnswered in the event stream.
    /// This is the correct order: clients must see the incoming call notification
    /// before any ownership/answered state events.
    #[tokio::test]
    async fn test_on_enter_call_incoming_before_call_answered() {
        let gateway = create_test_gateway();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        let session_id = {
            let mut gw = gateway.write().await;
            let identity = RwiIdentity {
                token: "tok".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            sid
        };

        let app = RwiApp::new("ctx".to_string(), Some(session_id), gateway.clone());

        app.send_event(crate::rwi::proto::RwiEvent::CallIncoming(crate::rwi::proto::CallIncomingData {
            call_id: "c-test".to_string(),
            context: "ctx".to_string(),
            caller: "1001".to_string(),
            callee: "2000".to_string(),
            direction: "inbound".to_string(),
            trunk: None,
            sip_headers: std::collections::HashMap::new(),
        })).await;

        app.send_event(crate::rwi::proto::RwiEvent::CallAnswered {
            call_id: "c-test".to_string(),
        }).await;

        let first = event_rx.try_recv().expect("first event should arrive");
        let first_str = serde_json::to_string(&first).unwrap();
        assert!(
            first_str.contains("call_incoming") || first_str.contains("CallIncoming"),
            "first event must be call_incoming, got: {first_str}"
        );

        let second = event_rx.try_recv().expect("second event should arrive");
        let second_str = serde_json::to_string(&second).unwrap();
        assert!(
            second_str.contains("call_answered") || second_str.contains("CallAnswered"),
            "second event must be call_answered, got: {second_str}"
        );
    }

    /// Verifies that when there is no session_id (anonymous context app),
    /// only CallIncoming is emitted — no CallAnswered.
    #[tokio::test]
    async fn test_on_enter_without_session_id_no_call_answered() {
        let gateway = create_test_gateway();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        {
            let mut gw = gateway.write().await;
            let identity = RwiIdentity { token: "sub".into(), scopes: vec![] };
            let session = gw.create_session(identity);
            let sid = session.read().await.id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            gw.subscribe(&sid, vec!["ctx-anon".to_string()]).await;
        }

        let app = RwiApp::new("ctx-anon".to_string(), None, gateway.clone());
        app.send_event(crate::rwi::proto::RwiEvent::CallIncoming(crate::rwi::proto::CallIncomingData {
            call_id: "c-anon".to_string(),
            context: "ctx-anon".to_string(),
            caller: "1002".to_string(),
            callee: "2001".to_string(),
            direction: "inbound".to_string(),
            trunk: None,
            sip_headers: std::collections::HashMap::new(),
        })).await;

        let ev = event_rx.try_recv().expect("CallIncoming should arrive");
        let ev_str = serde_json::to_string(&ev).unwrap();
        assert!(
            ev_str.contains("call_incoming") || ev_str.contains("CallIncoming"),
            "expected call_incoming, got: {ev_str}"
        );
        assert!(
            event_rx.try_recv().is_err(),
            "no CallAnswered should be emitted when there is no session_id"
        );
    }
}
