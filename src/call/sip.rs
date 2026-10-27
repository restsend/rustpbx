use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver};
use rsipstack::dialog::dialog_layer::DialogLayer;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{debug, info, warn};

pub(crate) const CALL_LEG_EVENT_CT: &str = "application/call-leg-event+json";

pub(crate) const ADMIN_CMD_CT: &str = "application/restsend-cmd+json";
pub(crate) const ADMIN_CMD_RELAY_CT: &str = "application/restsend-admin-relay+json";
pub(crate) const ADMIN_CMD_ANSWER_CT: &str = "application/restsend-cmd-answer+json";

pub(crate) const ADMIN_CMD_ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
pub(crate) const ADMIN_CMD_AGENT_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Clone)]
pub(crate) struct AdminCmdAnswer {
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) enum AdminCmdOutcome {
    NotRegistered,
    NoAnswer,
    Answered(AdminCmdAnswer),
}

/// The capability is carried in the offer, so pager events remain call-bound after CANCEL.
pub(crate) fn ring_timeout_offer(headers: &[rsipstack::sip::Header]) -> Option<String> {
    headers.iter().find_map(|header| {
        if !header.name().eq_ignore_ascii_case("Call-Info") {
            return None;
        }
        let (uri, purpose) = header.value().split_once(';')?;
        let uri = uri.trim().strip_prefix('<')?.strip_suffix('>')?;
        (purpose.trim() == "purpose=ring-timeout" && uri.starts_with("urn:uuid:"))
            .then(|| uri.to_string())
    })
}

pub(crate) fn send_call_leg_event(
    dialog_layer: &DialogLayer,
    endpoint: Arc<rsipstack::transaction::endpoint::EndpointInner>,
    offer: Box<rsipstack::dialog::invitation::InviteOption>,
    body: Vec<u8>,
) -> anyhow::Result<()> {
    send_pager_message(dialog_layer, endpoint, offer, body, CALL_LEG_EVENT_CT)
}

pub(crate) fn send_pager_message(
    _dialog_layer: &DialogLayer,
    endpoint: Arc<rsipstack::transaction::endpoint::EndpointInner>,
    mut offer: Box<rsipstack::dialog::invitation::InviteOption>,
    body: Vec<u8>,
    content_type: &str,
) -> anyhow::Result<()> {
    use rsipstack::sip::{HeadersExt, ToTypedHeader};
    use rsipstack::transaction::{
        key::{TransactionKey, TransactionRole},
        transaction::Transaction,
    };
    offer.call_id = Some(uuid::Uuid::new_v4().to_string());
    offer.content_type = Some(content_type.into());
    let mut request = _dialog_layer.make_invite_request(&offer)?;
    request.body = body;
    request.method = rsipstack::sip::Method::Message;
    let mut cseq = request.cseq_header()?.typed()?;
    cseq.method = rsipstack::sip::Method::Message;
    request.headers.unique_push(cseq.into());
    let key = TransactionKey::from_request(&request, TransactionRole::Client)?;
    let mut tx = Transaction::new_client(key, request, endpoint, None);
    tx.destination = offer.destination;
    // A pager transaction still reaches the phone after CANCEL. Never delay teardown
    // waiting for its response, and retain the stack's normal transaction retry policy.
    crate::utils::spawn(async move {
        if let Err(error) = tx.send().await {
            warn!(%error, "Pager MESSAGE send failed");
            return;
        }
        while let Some(message) = tx.receive().await {
            if let rsipstack::sip::SipMessage::Response(response) = message
                && response.status_code.code() >= 200
            {
                break;
            }
        }
    });
    Ok(())
}

pub(crate) async fn send_pager_message_awaiting_response(
    dialog_layer: &DialogLayer,
    endpoint: Arc<rsipstack::transaction::endpoint::EndpointInner>,
    mut offer: Box<rsipstack::dialog::invitation::InviteOption>,
    body: Vec<u8>,
    content_type: &str,
) -> anyhow::Result<Option<rsipstack::sip::Response>> {
    use rsipstack::sip::{HeadersExt, ToTypedHeader};
    use rsipstack::transaction::{
        key::{TransactionKey, TransactionRole},
        transaction::Transaction,
    };
    offer.call_id = Some(uuid::Uuid::new_v4().to_string());
    offer.content_type = Some(content_type.into());
    let mut request = dialog_layer.make_invite_request(&offer)?;
    request.body = body;
    request.method = rsipstack::sip::Method::Message;
    let mut cseq = request.cseq_header()?.typed()?;
    cseq.method = rsipstack::sip::Method::Message;
    request.headers.unique_push(cseq.into());
    let key = TransactionKey::from_request(&request, TransactionRole::Client)?;
    let mut tx = Transaction::new_client(key, request, endpoint, None);
    tx.destination = offer.destination;
    tx.send().await?;
    while let Some(message) = tx.receive().await {
        if let rsipstack::sip::SipMessage::Response(response) = message
            && response.status_code.code() >= 200
        {
            return Ok(Some(response));
        }
    }
    Ok(None)
}

fn response_content_type(response: &rsipstack::sip::Response) -> Option<String> {
    response.headers.iter().find_map(|h| match h {
        rsipstack::sip::Header::ContentType(ct) => Some(ct.value().to_string()),
        _ => None,
    })
}

pub(crate) async fn resolve_agent_registration(
    server: &crate::proxy::server::SipServerInner,
    agent: &str,
) -> Option<crate::call::Location> {
    let realm = server.proxy_config.load().select_realm("");
    for candidate in [
        crate::call::build_sip_uri(agent, &realm),
        crate::call::build_sip_uri(agent, "localhost"),
    ] {
        let Ok(uri) = candidate.parse::<rsipstack::sip::Uri>() else {
            continue;
        };
        let Ok(locations) = server.locator.lookup(&uri).await else {
            continue;
        };
        if let Some(location) = locations.into_iter().find(|location| {
            location.registered_aor.is_some() && location.destination.is_some()
        }) {
            return Some(location);
        }
    }
    None
}

pub(crate) async fn deliver_admin_cmd(
    server: &crate::proxy::server::SipServerInner,
    agent: &str,
    body: &[u8],
) -> anyhow::Result<AdminCmdOutcome> {
    deliver_admin_cmd_with_wait(server, agent, body, ADMIN_CMD_ANSWER_WAIT).await
}

pub(crate) async fn deliver_admin_cmd_with_wait(
    server: &crate::proxy::server::SipServerInner,
    agent: &str,
    body: &[u8],
    wait: std::time::Duration,
) -> anyhow::Result<AdminCmdOutcome> {
    let Some(location) = resolve_agent_registration(server, agent).await else {
        return Ok(AdminCmdOutcome::NotRegistered);
    };
    let home_is_self = location.home_proxy.as_ref().is_none_or(|home| {
        server
            .cluster_self_addr
            .clone()
            // Single-node registrations use the listener Contact as their home.
            .or_else(|| {
                server.default_contact_uri().and_then(|uri| uri.try_into().ok())
            })
            .as_ref()
            .is_some_and(|self_addr| home.addr == self_addr.addr)
    });
    if home_is_self {
        deliver_admin_cmd_at(server, &location, body, wait).await
    } else {
        relay_admin_cmd_to_home(server, &location, body, wait).await
    }
}

pub(crate) async fn deliver_admin_cmd_relayed(
    server: &crate::proxy::server::SipServerInner,
    agent: &str,
    body: &[u8],
) -> anyhow::Result<AdminCmdOutcome> {
    let Some(location) = resolve_agent_registration(server, agent).await else {
        return Ok(AdminCmdOutcome::NotRegistered);
    };
    let home_is_self = location.home_proxy.as_ref().is_none_or(|home| {
        server
            .cluster_self_addr
            .clone()
            .or_else(|| {
                server.default_contact_uri().and_then(|uri| uri.try_into().ok())
            })
            .as_ref()
            .is_some_and(|self_addr| home.addr == self_addr.addr)
    });
    if !home_is_self {
        return Ok(AdminCmdOutcome::NotRegistered);
    }
    deliver_admin_cmd_at(server, &location, body, ADMIN_CMD_AGENT_WAIT).await
}

async fn deliver_admin_cmd_at(
    server: &crate::proxy::server::SipServerInner,
    location: &crate::call::Location,
    body: &[u8],
    wait: std::time::Duration,
) -> anyhow::Result<AdminCmdOutcome> {
    let caller = location
        .registered_aor
        .clone()
        .unwrap_or_else(|| location.aor.clone());
    let contact = server
        .contact_uri_for_location(location)
        .ok_or_else(|| anyhow::anyhow!("No SIP Contact for admin cmd delivery"))?;
    let offer = Box::new(rsipstack::dialog::invitation::InviteOption {
        callee: location.aor.clone(),
        caller,
        contact,
        destination: location.destination.clone(),
        ..Default::default()
    });
    let response = match tokio::time::timeout(
        wait,
        send_pager_message_awaiting_response(
            &server.dialog_layer,
            server.endpoint.inner.clone(),
            offer,
            body.to_vec(),
            ADMIN_CMD_CT,
        ),
    )
    .await
    {
        Ok(Ok(Some(response))) => Some(response),
        Ok(Ok(None)) | Err(_) => {
            warn!(wait = ?wait, "admin cmd: agent did not answer in time");
            return Ok(AdminCmdOutcome::NoAnswer);
        }
        Ok(Err(error)) => return Err(error),
    };
    match response {
        Some(response) if response.status_code.code() == 200 => {
            Ok(AdminCmdOutcome::Answered(AdminCmdAnswer {
                content_type: response_content_type(&response),
                body: response.body().to_vec(),
            }))
        }
        Some(response) => {
            warn!(
                status = response.status_code.code(),
                "admin cmd answered with non-200; treating as no answer"
            );
            Ok(AdminCmdOutcome::NoAnswer)
        }
        None => Ok(AdminCmdOutcome::NoAnswer),
    }
}

async fn relay_admin_cmd_to_home(
    server: &crate::proxy::server::SipServerInner,
    location: &crate::call::Location,
    body: &[u8],
    wait: std::time::Duration,
) -> anyhow::Result<AdminCmdOutcome> {
    let Some(home) = location.home_proxy.clone() else {
        anyhow::bail!("relay requested but location has no home_proxy");
    };
    let caller = location
        .registered_aor
        .clone()
        .unwrap_or_else(|| location.aor.clone());
    let contact = server
        .contact_uri_for_location(location)
        .ok_or_else(|| anyhow::anyhow!("No SIP Contact for admin cmd relay"))?;
    let offer = Box::new(rsipstack::dialog::invitation::InviteOption {
        callee: location.aor.clone(),
        caller,
        contact,
        destination: Some(home),
        ..Default::default()
    });
    let response = match tokio::time::timeout(
        wait,
        send_pager_message_awaiting_response(
            &server.dialog_layer,
            server.endpoint.inner.clone(),
            offer,
            body.to_vec(),
            ADMIN_CMD_RELAY_CT,
        ),
    )
    .await
    {
        Ok(Ok(Some(response))) => Some(response),
        Ok(Ok(None)) | Err(_) => {
            warn!("admin cmd relay: home node did not answer in time");
            return Ok(AdminCmdOutcome::NoAnswer);
        }
        Ok(Err(error)) => return Err(error),
    };
    match response {
        Some(response) if response.status_code.code() == 200 => {
            Ok(AdminCmdOutcome::Answered(AdminCmdAnswer {
                content_type: response_content_type(&response),
                body: response.body().to_vec(),
            }))
        }
        Some(response) if response.status_code.code() == 404 => Ok(AdminCmdOutcome::NotRegistered),
        Some(response) => {
            warn!(
                status = response.status_code.code(),
                "admin cmd relay answered with unexpected status"
            );
            Ok(AdminCmdOutcome::NoAnswer)
        }
        None => Ok(AdminCmdOutcome::NoAnswer),
    }
}

pub struct DialogStateReceiverGuard {
    pub(super) dialog_layer: Arc<DialogLayer>,
    pub(super) receiver: Option<DialogStateReceiver>,
    pub(super) dialog_id: Option<DialogId>,
}

impl DialogStateReceiverGuard {
    pub fn new(dialog_layer: Arc<DialogLayer>, receiver: DialogStateReceiver) -> Self {
        Self {
            dialog_layer,
            receiver: Some(receiver),
            dialog_id: None,
        }
    }
    pub async fn recv(&mut self) -> Option<DialogState> {
        let receiver = self.receiver.as_mut()?;
        let state = receiver.recv().await;
        if let Some(ref s) = state {
            self.dialog_id = Some(s.id().clone());
        }
        state
    }

    pub fn take_receiver(&mut self) -> Option<DialogStateReceiver> {
        self.receiver.take()
    }

    pub fn disarm(&mut self) {
        self.dialog_id = None;
    }

    pub fn set_dialog_id(&mut self, dialog_id: DialogId) {
        self.dialog_id = Some(dialog_id);
    }

    fn take_dialog(&mut self) -> Option<Dialog> {
        let id = self.dialog_id.take()?;

        if let Some(dialog) = self.dialog_layer.get_dialog(&id) {
            info!(%id, "dialog removed on  drop");
            self.dialog_layer.remove_dialog(&id);
            return Some(dialog);
        }
        None
    }

    pub async fn drop_async(&mut self) {
        if let Some(dialog) = self.take_dialog()
            && let Err(e) = dialog.hangup().await
        {
            warn!(id=%dialog.id(), "error hanging up dialog on drop: {}", e);
        }
    }
}

impl Drop for DialogStateReceiverGuard {
    fn drop(&mut self) {
        if let Some(dialog) = self.take_dialog() {
            if tokio::runtime::Handle::try_current().is_ok() {
                crate::utils::spawn(async move {
                    if let Err(e) = dialog.hangup().await {
                        warn!(id=%dialog.id(), "error hanging up dialog on drop: {}", e);
                    }
                });
            }
        }
    }
}

pub struct ServerDialogGuard {
    pub dialog_layer: Arc<DialogLayer>,
    pub id: DialogId,
}

impl ServerDialogGuard {
    pub fn new(dialog_layer: Arc<DialogLayer>, id: DialogId) -> Self {
        Self { dialog_layer, id }
    }
}

impl Drop for ServerDialogGuard {
    fn drop(&mut self) {
        let dlg = match self.dialog_layer.get_dialog(&self.id) {
            Some(dlg) => {
                self.dialog_layer.remove_dialog(&self.id);
                match dlg {
                    Dialog::Invite(dlg) => dlg,
                    _ => return,
                }
            }
            _ => return,
        };
        let state = dlg.state();
        if state.is_terminated() {
            return;
        }

        if tokio::runtime::Handle::try_current().is_ok() {
            crate::utils::spawn(async move {
                if state.can_cancel() {
                    dlg.reject(None, None).ok();
                } else {
                    dlg.bye_with_headers(None).await.ok();
                }
            });
        }
    }
}

pub struct ClientDialogGuard {
    pub dialog_layer: Arc<DialogLayer>,
    pub id: DialogId,
}

impl ClientDialogGuard {
    pub fn new(dialog_layer: Arc<DialogLayer>, id: DialogId) -> Self {
        Self { dialog_layer, id }
    }

    pub fn id(&self) -> &DialogId {
        &self.id
    }
}

impl Drop for ClientDialogGuard {
    fn drop(&mut self) {
        let dlg = match self.dialog_layer.get_dialog(&self.id) {
            Some(dlg) => {
                debug!(%self.id, state = %dlg.state(), "client dialog removed on drop");
                self.dialog_layer.remove_dialog(&self.id);
                dlg
            }
            None => return,
        };

        if dlg.state().is_terminated() {
            return;
        }

        // Guard like the sibling Drop impls: `tokio::spawn` panics outside a
        // runtime (unit tests, process teardown ordering).
        if tokio::runtime::Handle::try_current().is_ok() {
            crate::utils::spawn(async move {
                if let Err(e) = dlg.hangup().await {
                    warn!(id=%dlg.id(), "error hanging up client dialog on drop: {}", e);
                }
            });
        }
    }
}

pub fn spawn_client_dialog_guard(
    dialog_layer: Arc<DialogLayer>,
    dialog_id: DialogId,
    mut watch_rx: watch::Receiver<Option<DialogState>>,
) {
    crate::utils::spawn(async move {
        let _guard = ClientDialogGuard::new(dialog_layer, dialog_id);
        loop {
            if watch_rx.changed().await.is_err() {
                break;
            }
            let state = watch_rx.borrow();
            if state
                .as_ref()
                .map_or(true, |s| matches!(s, DialogState::Terminated(..)))
            {
                break;
            }
        }
    });
}
