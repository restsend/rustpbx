use super::{AppAction, ApplicationContext, CallApp, CallAppType, CallController};
use crate::rwi::{CallCreated, RwiGatewayRef};
use async_trait::async_trait;

/// Holds an application-routed call while an RWI client controls it.
pub struct RwiApp {
    context_name: String,
    gateway: RwiGatewayRef,
}

impl RwiApp {
    pub fn new(context_name: String, gateway: RwiGatewayRef) -> Self {
        Self { context_name, gateway }
    }
}

#[async_trait]
impl CallApp for RwiApp {
    fn name(&self) -> &str { "rwi" }

    fn app_type(&self) -> CallAppType { CallAppType::Custom }

    async fn on_enter(
        &mut self,
        _controller: &mut CallController,
        context: &ApplicationContext,
    ) -> anyhow::Result<AppAction> {
        let event = CallCreated {
            call_id: context.call_info.session_id.clone(),
            context: self.context_name.clone(),
            caller: context.call_info.caller.clone(),
            callee: context.call_info.callee.clone(),
            sip_headers: context.call_info.sip_headers.clone(),
            trunk: None,
            caller_name: None,
            callee_name: None,
            called_phone: None,
            app_id: None,
            routing_target: None,
            uuid: None,
            routing_path: None,
        };
        self.gateway.read().fan_out(&self.context_name, &event);
        Ok(AppAction::Continue)
    }
}
