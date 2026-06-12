use crate::call::RouteContext;
use crate::call::app::CallController;
use crate::call::app::{AppAction, ApplicationContext, CallApp, CallAppType};
use crate::rwi::RwiGatewayRef;
use crate::rwi::RwiEventSpec;
use crate::rwi::gateway::SessionId;
use crate::rwi::session::OwnershipMode;
use async_trait::async_trait;

pub const RWI_APP_NAME: &str = "rwi";

#[derive(Clone)]
pub struct RwiAddon {
    gateway: RwiGatewayRef,
}

impl RwiAddon {
    pub fn new(gateway: RwiGatewayRef) -> Self {
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

        Some(Box::new(RwiApp::new(
            context_name,
            session_id,
            self.gateway.clone(),
        )))
    }
}

pub struct RwiApp {
    context_name: String,
    session_id: Option<SessionId>,
    gateway: RwiGatewayRef,
    owned: bool,
    /// The call_id of the call this app is handling, set in `on_enter`.
    owned_call_id: Option<String>,
    /// Track ID of the currently playing audio, if any.
    #[allow(dead_code)]
    current_track_id: Option<String>,
    /// If `true`, the next DTMF digit will interrupt the current playback.
    #[allow(dead_code)]
    interrupt_on_dtmf: bool,
}

impl RwiApp {
    pub fn new(
        context_name: String,
        session_id: Option<SessionId>,
        gateway: RwiGatewayRef,
    ) -> Self {
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

    async fn send_typed_event<E: RwiEventSpec>(&self, event: &E) {
        let gw = self.gateway.read();
        if let Some(session_id) = &self.session_id {
            gw.send_to_session(session_id, event);
        }
        gw.fan_out_excluding(&self.context_name, event, self.session_id.as_ref());
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

        // Populate CallMetaStore for event enrichment
        let meta_store = self.gateway.read().meta_store.clone();
        meta_store
            .insert(
                call_id.clone(),
                crate::rwi::proto::CallMeta {
                    caller: Some(context.call_info.caller.clone()),
                    callee: Some(context.call_info.callee.clone()),
                    direction: Some(context.call_info.direction.clone()),
                    caller_name: Some(context.call_info.caller.clone()),
                    callee_name: Some(context.call_info.callee.clone()),
                    trunk: context.call_info.sip_headers.get("X-Trunk").cloned(),
                    app_id: None,
                    routing_target: None,
                    agent_id: None,
                    agent_name: None,
                },
            )
            .await;

        self.send_typed_event(&crate::rwi::CallIncoming {
            call_id: call_id.clone(),
            context: self.context_name.clone(),
            caller: context.call_info.caller.clone(),
            callee: context.call_info.callee.clone(),
            dial_direction: context.call_info.direction.clone(),
            trunk: None,
            sip_headers: context.call_info.sip_headers.clone(),
            root_call_id: None,
            caller_name: None,
            callee_name: None,
            called_phone: None,
            app_id: None,
            routing_target: None,
            uuid: None,
            routing_path: None,
        })
        .await;

        if let Some(session_id) = &self.session_id {
            let claim_ok = {
                let mut gateway = self.gateway.write();
                gateway
                    .claim_call_ownership(session_id, call_id.clone(), OwnershipMode::Control)
                    .is_ok()
            };

            if claim_ok {
                self.owned = true;
                self.owned_call_id = Some(call_id.clone());
                self.send_typed_event(&crate::rwi::CallAnswered {
                    call_id: call_id.clone(),
                })
                .await;
            }
        }

        Ok(AppAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rwi::auth::RwiIdentity;
    use crate::rwi::gateway::RwiGateway;
    use parking_lot::RwLock;
    use std::sync::Arc;

    fn create_test_gateway() -> RwiGatewayRef {
        Arc::new(RwLock::new(RwiGateway::new()))
    }

    /// Verifies that when there is no session_id (anonymous context app),
    /// only CallIncoming is emitted — no CallAnswered.
    #[tokio::test]
    async fn test_on_enter_without_session_id_no_call_answered() {
        let gateway = create_test_gateway();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        {
            let mut gw = gateway.write();
            let identity = RwiIdentity {
                token: "sub".into(),
                scopes: vec![],
            };
            let session = gw.create_session(identity);
            let sid = session.read().id.clone();
            gw.set_session_event_sender(&sid, event_tx);
            gw.subscribe(&sid, vec!["ctx-anon".to_string()], None);
        }

        let app = RwiApp::new("ctx-anon".to_string(), None, gateway.clone());
        app.send_typed_event(&crate::rwi::CallIncoming {
            call_id: "c-anon".to_string(),
            context: "ctx-anon".to_string(),
            caller: "1002".to_string(),
            callee: "2001".to_string(),
            dial_direction: "inbound".to_string(),
            trunk: None,
            sip_headers: std::collections::HashMap::new(),
            root_call_id: None,
            caller_name: None,
            callee_name: None,
            called_phone: None,
            app_id: None,
            routing_target: None,
            uuid: None,
            routing_path: None,
        })
        .await;

        let ev = event_rx.try_recv().expect("CallIncoming should arrive");
        let ev_str = serde_json::to_string(&ev).unwrap();
        assert!(
            ev_str.contains("dial_direction"),
            "expected call_incoming, got: {ev_str}"
        );
        assert!(
            event_rx.try_recv().is_err(),
            "no CallAnswered should be emitted when there is no session_id"
        );
    }
}
