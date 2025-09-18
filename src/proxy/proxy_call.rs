use crate::{
    call::{DialStrategy, Dialplan, FailureAction, Location, TransactionCookie},
    callrecord::{CallRecord, CallRecordSender},
};
use anyhow::{Result, anyhow};
use rsip::StatusCode;
use rsipstack::{
    dialog::{
        DialogId,
        dialog::{DialogState, DialogStateReceiver, TerminatedReason},
        dialog_layer::DialogLayer,
        invitation::InviteOption,
        server_dialog::ServerInviteDialog,
    },
    rsip_ext::RsipResponseExt,
    transaction::transaction::Transaction,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, mpsc::unbounded_channel};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct ProxyCallInner {
    pub start_time: Instant,
    pub session_id: String,
    pub dialplan: Dialplan,
    pub cookie: TransactionCookie,
    pub dialog_layer: Arc<DialogLayer>,
    pub cancel_token: CancellationToken,
    pub call_record_sender: Option<CallRecordSender>,
}

#[derive(Clone)]
pub struct ProxyCall {
    inner: Arc<ProxyCallInner>,
}

pub struct ProxyCallBuilder {
    cookie: TransactionCookie,
    dialplan: Option<Dialplan>,
    cancel_token: Option<CancellationToken>,
    call_record_sender: Option<CallRecordSender>,
    original_sdp_offer: Option<String>,
}

impl ProxyCallBuilder {
    pub fn new(cookie: TransactionCookie) -> Self {
        Self {
            cookie,
            dialplan: None,
            cancel_token: None,
            call_record_sender: None,
            original_sdp_offer: None,
        }
    }

    pub fn with_dialplan(mut self, dialplan: Dialplan) -> Self {
        self.dialplan = Some(dialplan);
        self
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub fn with_call_record_sender(mut self, sender: CallRecordSender) -> Self {
        self.call_record_sender = Some(sender);
        self
    }

    pub fn with_original_sdp_offer(mut self, sdp_offer: String) -> Self {
        self.original_sdp_offer = Some(sdp_offer);
        self
    }

    pub fn build(self, dialog_layer: Arc<DialogLayer>) -> ProxyCall {
        let dialplan = self.dialplan.unwrap_or_default();
        let cancel_token = self.cancel_token.unwrap_or_default();

        let session_id = dialplan
            .session_id
            .as_ref()
            .map(|s| s.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let inner = Arc::new(ProxyCallInner {
            start_time: Instant::now(),
            session_id,
            dialplan,
            cookie: self.cookie,
            dialog_layer,
            cancel_token,
            call_record_sender: self.call_record_sender,
        });

        ProxyCall { inner }
    }
}

impl Drop for ProxyCall {
    fn drop(&mut self) {
        debug!(session_id = self.inner.session_id, "proxy call dropped");
    }
}

struct CalleeContext {
    dialog_layer: Arc<DialogLayer>,
    session_id: String,
    start_time: Instant,
    ring_time: Option<Instant>,
    answer_time: Option<Instant>,
    hangup_time: Option<Instant>,
    offer: Option<String>,
    answer: Option<String>,
    last_failed: Option<(StatusCode, Option<String>)>,
    terminated_reason: Option<TerminatedReason>,
}

impl CalleeContext {
    pub fn new(session_id: String, dialog_layer: Arc<DialogLayer>) -> Self {
        Self {
            dialog_layer,
            session_id,
            start_time: Instant::now(),
            ring_time: None,
            answer_time: None,
            hangup_time: None,
            offer: None,
            answer: None,
            last_failed: None,
            terminated_reason: None,
        }
    }

    pub async fn process_callee_invite(
        &mut self,
        ctx: &CallerContext,
        invite_option: InviteOption,
    ) -> Result<(), (StatusCode, Option<String>)> {
        let (state_sender, mut state_receiver) = unbounded_channel::<DialogState>();
        let dialog_id = Arc::new(Mutex::new(None::<DialogId>));

        let callee_state_loop = async {
            while let Some(state) = state_receiver.recv().await {
                match state {
                    DialogState::Calling(_) => {}
                    DialogState::Trying(id) => {
                        dialog_id.lock().unwrap().replace(id);
                    }
                    DialogState::Early(id, response) => {
                        dialog_id.lock().unwrap().replace(id);
                        if self.ring_time.is_none() {
                            self.ring_time = Some(Instant::now());
                        }
                        if response.status_code == StatusCode::SessionProgress {
                            if let Ok(s) = String::from_utf8(response.body().clone()) {
                                self.answer = Some(s); // provisional answer
                            }
                        }
                        match self.answer {
                            Some(ref body) => {
                                ctx.on_callee_early_media(body).await.ok();
                            }
                            None => {
                                ctx.on_callee_ringing().await.ok();
                            }
                        }
                    }
                    DialogState::WaitAck(_, _) => {}
                    DialogState::Confirmed(id, response) => {
                        dialog_id.lock().unwrap().replace(id);
                        if self.answer_time.is_none() {
                            self.answer_time = Some(Instant::now());
                        }
                        match response.status_code.kind() {
                            rsip::StatusCodeKind::Successful => {
                                if let Ok(s) = String::from_utf8(response.body().clone()) {
                                    ctx.on_callee_confirmed(&s).await.ok();
                                    self.answer = Some(s);
                                }
                            }
                            _ => {
                                let reason = response.reason_phrase().map(|s| s.to_string());
                                self.last_failed = Some((response.status_code, reason));
                            }
                        }
                    }
                    DialogState::Updated(_, _) => {}
                    DialogState::Notify(_, _) => {}
                    DialogState::Info(_, _) => {}
                    DialogState::Options(_, _) => {}
                    DialogState::Terminated(_, terminated_reason) => {
                        ctx.on_callee_terminated(&terminated_reason).await.ok();
                        self.terminated_reason = Some(terminated_reason);
                        break;
                    }
                }
            }
        };

        let mut caller_state = ctx.state.subscribe();
        let caller_state_loop = async {
            while let Ok(state) = caller_state.recv().await {
                match state {
                    DialogState::Terminated(_, _) => {
                        if let Some(dlg_id) = { dialog_id.lock().unwrap().clone() } {
                            if let Some(dlg) = self.dialog_layer.get_dialog(&dlg_id) {
                                dlg.hangup().await.ok();
                            }
                        }
                        break;
                    }
                    _ => {}
                }
            }
        };

        let (_, _, r) = tokio::join!(
            caller_state_loop,
            callee_state_loop,
            self.dialog_layer.do_invite(invite_option, state_sender)
        );

        if let Some(dlg_id) = { dialog_id.lock().unwrap().take() } {
            if let Some(dlg) = self.dialog_layer.get_dialog(&dlg_id) {
                dlg.hangup().await.ok();
            }
        }

        match r {
            Ok(_) => Ok(()),
            Err(e) => match e {
                rsipstack::Error::DialogError(reason, _, status_code) => {
                    self.last_failed = Some((status_code.clone(), Some(reason.clone())));
                    Err((status_code, Some(reason)))
                }
                _ => Err((StatusCode::ServerInternalError, Some(e.to_string()))),
            },
        }
    }
}

struct CallerContext {
    dialog: ServerInviteDialog,
    state: broadcast::Sender<DialogState>,
    last_failed: Mutex<Option<(StatusCode, Option<String>)>>,
}

impl CallerContext {
    pub async fn on_callee_ringing(&self) -> Result<()> {
        self.dialog.ringing(None, None).map_err(Into::into)
    }

    pub async fn on_callee_early_media(&self, body: &String) -> Result<()> {
        let (headers, body) = {
            let headers = vec![rsip::Header::ContentType(
                "application/sdp".to_string().into(),
            )];
            (Some(headers), Some(body.as_bytes().to_vec()))
        };
        self.dialog.ringing(headers, body).map_err(Into::into)
    }

    pub async fn on_callee_confirmed(&self, body: &String) -> Result<()> {
        let (headers, body) = {
            let headers = vec![rsip::Header::ContentType(
                "application/sdp".to_string().into(),
            )];
            (Some(headers), Some(body.as_bytes().to_vec()))
        };
        self.dialog.accept(headers, body).map_err(Into::into)
    }

    pub async fn on_callee_terminated(&self, _reason: &TerminatedReason) -> Result<()> {
        if self.dialog.state().is_confirmed() {
            return self.dialog.bye().await.map_err(Into::into);
        }
        Ok(())
    }

    pub async fn cleanup(&self) {
        let failed = self.last_failed.lock().unwrap().take();
        if let Some((code, reason)) = failed {
            self.dialog.reject(Some(code), reason).ok();
        }
        self.dialog.bye().await.ok();
    }
}

impl ProxyCall {
    pub fn elapsed(&self) -> Duration {
        self.inner.start_time.elapsed()
    }

    pub fn id(&self) -> &String {
        &self.inner.session_id
    }

    async fn handle_server_dialog(
        &self,
        ctx: &CallerContext,
        mut state_receiver: DialogStateReceiver,
    ) -> Result<()> {
        while let Some(state) = state_receiver.recv().await {
            if let Err(_) = ctx.state.send(state.clone()) {
                break;
            }
        }
        Ok(())
    }

    async fn execute_dialplan(&self, ctx: &CallerContext) -> Result<()> {
        let dialplan = &self.inner.dialplan;

        if dialplan.is_empty() {
            return Err(anyhow!("Dialplan has no targets"));
        }

        let r = match &dialplan.targets {
            DialStrategy::Sequential(targets) => {
                self.execute_sequential_dialing(ctx, targets).await
            }
            DialStrategy::Parallel(targets) => self.execute_parallel_dialing(ctx, targets).await,
        };
        match r {
            Ok(_) => Ok(()),
            Err(_) => self.execute_failure_action(ctx).await,
        }
    }

    async fn execute_sequential_dialing(
        &self,
        ctx: &CallerContext,
        targets: &[Location],
    ) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            target_count = targets.len(),
            "starting sequential dialing"
        );

        for target in targets.iter() {
            match self.try_dial_target(ctx, target).await {
                Ok(_) => {
                    return Ok(());
                }
                Err((status_code, reason)) => {
                    warn!(
                        session_id = self.inner.session_id,
                        target = %target,
                        %status_code,
                        reason = ?reason,
                        "Sequential dial failed, trying next target"
                    );
                    ctx.last_failed
                        .lock()
                        .unwrap()
                        .replace((status_code, reason));
                }
            }
        }

        Err(anyhow!("All sequential targets failed"))
    }

    async fn execute_parallel_dialing(
        &self,
        _ctx: &CallerContext,
        targets: &[Location],
    ) -> Result<()> {
        info!(
            session_id = self.inner.session_id,
            target_count = targets.len(),
            "Starting parallel dialing"
        );
        Ok(())
    }

    async fn build_callee_offer(&self, _ctx: &CallerContext) -> Option<String> {
        None
    }

    /// Initiate actual SIP call to target
    async fn try_dial_target(
        &self,
        ctx: &CallerContext,
        target: &Location,
    ) -> Result<(), (rsip::StatusCode, Option<String>)> {
        let caller = match self.inner.dialplan.caller.clone() {
            Some(caller) => caller,
            None => {
                return Err((
                    StatusCode::ServerInternalError,
                    Some("No caller specified in dialplan".to_string()),
                ));
            }
        };

        let (content_type, offer) = match self.build_callee_offer(ctx).await {
            Some(offer) => (
                Some("application/sdp".to_string()),
                Some(offer.as_bytes().to_vec()),
            ),
            None => (None, None),
        };

        let invite_option = InviteOption {
            callee: target.aor.clone(),
            caller: caller.clone(),
            content_type,
            offer,
            destination: Some(target.destination.clone()),
            contact: self
                .inner
                .dialplan
                .caller_contact
                .clone()
                .map(|c| c.uri)
                .unwrap_or_else(|| caller),
            credential: target.credential.clone(),
            headers: target.headers.clone(),
        };
        let mut leg = CalleeContext::new(
            self.inner.session_id.clone(),
            self.inner.dialog_layer.clone(),
        );
        leg.process_callee_invite(ctx, invite_option).await
    }

    /// Execute failure action based on dialplan configuration
    async fn execute_failure_action(&self, ctx: &CallerContext) -> Result<()> {
        let failure_action = &self.inner.dialplan.failure_action;
        match failure_action {
            FailureAction::Hangup(status_code) => {
                if let Some(code) = status_code {
                    ctx.dialog.reject(Some(code.clone()), None).ok();
                }
            }
            FailureAction::PlayThenHangup {
                audio_file,
                status_code,
            } => {
                let _ = audio_file;
                ctx.dialog.reject(Some(status_code.clone()), None).ok();
            }
            FailureAction::Transfer(destination) => {
                self.try_transfer(ctx, destination).await;
            }
        }

        Ok(())
    }
    async fn try_transfer(&self, _ctx: &CallerContext, _destination: &String) {
        todo!()
    }

    pub async fn process(&self, tx: &mut Transaction) -> Result<()> {
        let (server_state_sender, server_state_receiver) = unbounded_channel::<DialogState>();
        let mut server_dlg = self.inner.dialog_layer.get_or_create_server_invite(
            tx,
            server_state_sender,
            None,
            None,
        )?;

        let caller_state = broadcast::Sender::new(16);
        let ctx = CallerContext {
            dialog: server_dlg.clone(),
            state: caller_state,
            last_failed: Mutex::new(None),
        };

        let (_, r, _) = tokio::join! {
            self.handle_server_dialog(&ctx, server_state_receiver),
            self.execute_dialplan(&ctx),
            server_dlg.handle(tx),
        };

        ctx.cleanup().await;

        if let Some(ref sender) = self.inner.call_record_sender {
            let callrecord = self.build_callrecord(&ctx);
            sender.send(callrecord).ok();
        }
        r
    }

    fn build_callrecord(&self, _ctx: &CallerContext) -> CallRecord {
        todo!()
    }
}
