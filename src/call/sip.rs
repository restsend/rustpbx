use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{Dialog, DialogState, DialogStateReceiver};
use rsipstack::dialog::dialog_layer::DialogLayer;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{debug, info, warn};

pub(crate) const CALL_LEG_EVENT_CT: &str = "application/call-leg-event+json";

/// Admin-command pager relay（console 管理指令的节点间中继）。与客户端
/// 信封同 CT（`application/restsend-cmd+json`）；中继跳（node→home node）
/// 用 RELAY CT 标记，home 节点按 cluster-peer 信任后终端投递。
pub(crate) const ADMIN_CMD_CT: &str = "application/restsend-cmd+json";
pub(crate) const ADMIN_CMD_RELAY_CT: &str = "application/restsend-admin-relay+json";

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

/// Out-of-dialog pager MESSAGE（RFC 3428）通用发送：目标地址/呼出身份由
/// `offer` 携带（callee=AOR、destination=传输地址、contact=本端）。用于
/// ring-timeout 通知（CALL_LEG_EVENT_CT）与 cc 管理指令投递/节点间中继。
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

/// 按坐席分机解析注册位置（realm 候选：配置 realm → localhost 归一）。
/// 命中要求 registered_aor + destination 齐备；`terminal_only` 时还要求
/// home_proxy 为本节点（跨节点位置交由调用方走中继）。
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

/// 管理指令投递入口（AMI 侧调用）：解析坐席注册 → home 在本节点（或无
/// home）终端 pager、home 在对端 → RELAY 中继给 home 节点（其
/// `handle_message` 信任 cluster peer 后终端投递）。返回 Ok(false) =
/// 集群内查无该坐席注册。
pub(crate) async fn deliver_admin_cmd(
    server: &crate::proxy::server::SipServerInner,
    agent: &str,
    body: &[u8],
) -> anyhow::Result<bool> {
    let Some(location) = resolve_agent_registration(server, agent).await else {
        return Ok(false);
    };
    let home_is_self = location.home_proxy.as_ref().is_none_or(|home| {
        server
            .cluster_self_addr
            .as_ref()
            .is_some_and(|self_addr| home.addr == self_addr.addr)
    });
    if home_is_self {
        deliver_admin_cmd_at(server, &location, body).await
    } else {
        relay_admin_cmd_to_home(server, &location, body).await?;
        Ok(true)
    }
}

/// RELAY 终端投递（proxy 分支专用）：仅当本节点就是坐席 home 时投递；
/// locator 中途漂移（home 指向别处）一律 Ok(false) → 404——中继消息
/// 不做二次转发（与 ring-timeout 同一终端语义）。
pub(crate) async fn deliver_admin_cmd_relayed(
    server: &crate::proxy::server::SipServerInner,
    agent: &str,
    body: &[u8],
) -> anyhow::Result<bool> {
    let Some(location) = resolve_agent_registration(server, agent).await else {
        return Ok(false);
    };
    let home_is_self = location.home_proxy.as_ref().is_none_or(|home| {
        server
            .cluster_self_addr
            .as_ref()
            .is_some_and(|self_addr| home.addr == self_addr.addr)
    });
    if !home_is_self {
        return Ok(false);
    }
    deliver_admin_cmd_at(server, &location, body).await
}

/// 终端投递：pager MESSAGE（客户端信封 CT）直达本节点坐席连接。
pub(crate) async fn deliver_admin_cmd_at(
    server: &crate::proxy::server::SipServerInner,
    location: &crate::call::Location,
    body: &[u8],
) -> anyhow::Result<bool> {
    // From host = 坐席注册 AOR 的 host——客户端门控按「From host == 自身
    // registrar host」校验（admin_cmd::host_of_uri），必须同域。
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
    send_pager_message(
        &server.dialog_layer,
        server.endpoint.inner.clone(),
        offer,
        body.to_vec(),
        ADMIN_CMD_CT,
    )?;
    Ok(true)
}

/// 节点间中继：把指令 pager 到坐席 home 节点（RELAY CT），home 节点
/// `handle_message` 信任 cluster peer 后走 [`deliver_admin_cmd_at`]。
pub(crate) async fn relay_admin_cmd_to_home(
    server: &crate::proxy::server::SipServerInner,
    location: &crate::call::Location,
    body: &[u8],
) -> anyhow::Result<()> {
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
    send_pager_message(
        &server.dialog_layer,
        server.endpoint.inner.clone(),
        offer,
        body.to_vec(),
        ADMIN_CMD_RELAY_CT,
    )?;
    Ok(())
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
