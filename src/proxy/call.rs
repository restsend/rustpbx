use crate::config::ProxyConfig;

use super::{server::SipServerRef, ProxyAction, ProxyModule};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rsip::headers::UntypedHeader;
use rsip::prelude::{HeadersExt, ToTypedHeader};
use rsipstack::dialog::DialogId;
use rsipstack::header_pop;
use rsipstack::rsip_ext::{extract_uri_from_contact, RsipHeadersExt};
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::SipAddr;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::select;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
struct UserContact {
    username: String,
    destination: SipAddr,
    last_seen: Instant,
}

#[derive(Clone)]
struct Session {
    dialog_id: DialogId,
    last_activity: Instant,
    parties: (String, String), // (caller, callee)
}

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    users: Arc<Mutex<HashMap<String, UserContact>>>,
    sessions: Arc<Mutex<HashMap<DialogId, Session>>>,
    session_timeout: Duration,
    options_interval: Duration,
}

#[derive(Clone)]
pub struct CallModule {
    inner: Arc<CallModuleInner>,
}

impl CallModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = CallModule::new(config, server);
        Ok(Box::new(module))
    }

    pub fn new(config: Arc<ProxyConfig>, server: SipServerRef) -> Self {
        // Default session timeout to 5 minutes and OPTIONS interval to 30 seconds
        // These could be configured from ProxyConfig
        let session_timeout = Duration::from_secs(300);
        let options_interval = Duration::from_secs(30);

        let inner = Arc::new(CallModuleInner {
            config,
            users: Arc::new(Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            session_timeout,
            options_interval,
        });

        let module = Self { inner };

        // Spawn background task for checking session timeouts
        module.start_session_monitor(server);

        module
    }

    fn start_session_monitor(&self, server: SipServerRef) {
        let inner = self.inner.clone();
        let endpoint = server.cancel_token.clone();

        tokio::spawn(async move {
            let mut interval = interval(inner.options_interval);
            loop {
                tokio::select! {
                    _ = endpoint.cancelled() => {
                        info!("Call module session monitor shutting down");
                        break;
                    },
                    _ = interval.tick() => {
                        Self::check_sessions(&inner).await;
                    }
                }
            }
        });
    }

    async fn check_sessions(inner: &CallModuleInner) {
        let now = Instant::now();
        let mut expired_sessions = Vec::new();

        // First pass: identify expired sessions
        {
            let sessions = inner.sessions.lock().unwrap();
            for (dialog_id, session) in sessions.iter() {
                if now.duration_since(session.last_activity) > inner.session_timeout {
                    expired_sessions.push(dialog_id.clone());
                }
            }
        }

        // Second pass: remove expired sessions
        for dialog_id in expired_sessions {
            info!("Session timeout for dialog: {}", dialog_id);
            inner.sessions.lock().unwrap().remove(&dialog_id);
        }
    }

    fn update_user_from_request(&self, req: &rsip::Request) -> Result<()> {
        let contact = match extract_uri_from_contact(req.contact_header()?.value()) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to extract contact: {}", e);
                return Err(anyhow!("Invalid contact header"));
            }
        };

        let via = req.via_header()?.typed()?;

        let mut destination = SipAddr {
            r#type: via.uri.transport().cloned(),
            addr: contact.host_with_port,
        };

        via.params.iter().for_each(|param| match param {
            rsip::Param::Transport(t) => {
                destination.r#type = Some(t.clone());
            }
            rsip::Param::Received(r) => match r.value().try_into() {
                Ok(addr) => destination.addr.host = addr,
                Err(_) => {}
            },
            rsip::Param::Other(o, Some(v)) => {
                if o.value().eq_ignore_ascii_case("rport") {
                    match v.value().try_into() {
                        Ok(port) => destination.addr.port = Some(port),
                        Err(_) => {}
                    }
                }
            }
            _ => {}
        });

        let username = req
            .from_header()?
            .uri()?
            .user()
            .unwrap_or_default()
            .to_string();

        let user_contact = UserContact {
            username: username.clone(),
            destination,
            last_seen: Instant::now(),
        };

        self.inner
            .users
            .lock()
            .unwrap()
            .insert(username, user_contact);
        Ok(())
    }

    async fn handle_invite(&self, tx: &mut Transaction) -> Result<()> {
        // Update the user's contact information from the INVITE
        self.update_user_from_request(&tx.original)?;

        let caller = tx.original.from_header()?.uri()?.to_string();
        let callee = tx
            .original
            .to_header()?
            .uri()?
            .auth
            .map(|a| a.user)
            .unwrap_or_default();

        let target = {
            let users = self.inner.users.lock().unwrap();
            users.get(&callee).cloned()
        };

        let target = match target {
            Some(u) => u,
            None => {
                info!("User not found: {}", callee);
                tx.reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e))?;
                // Wait for ACK
                while let Some(msg) = tx.receive().await {
                    match msg {
                        rsip::message::SipMessage::Request(req) => match req.method {
                            rsip::Method::Ack => {
                                debug!("Received ACK for 404 Not Found");
                                break;
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                return Ok(());
            }
        };

        // Create a copy of the original request to forward
        let mut inv_req = tx.original.clone();

        // Add our Via header
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        inv_req.headers.push_front(via.into());

        // Add Record-Route if supported
        if let Ok(record_route) = tx.endpoint_inner.get_record_route() {
            inv_req.headers.push_front(record_route.into());
        }

        // Create a client transaction for the forwarded INVITE
        let key = TransactionKey::from_request(&inv_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;

        info!("Forwarding INVITE: {} -> {}", caller, target.destination);

        let mut inv_tx = Transaction::new_client(key, inv_req, tx.endpoint_inner.clone(), None);
        inv_tx.destination = Some(target.destination);

        // Send the INVITE
        inv_tx.send().await.map_err(|e| anyhow!(e))?;

        // Create a dialog
        let dialog_id = match DialogId::try_from(&tx.original) {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to create dialog ID: {}", e);
                return tx
                    .reply(rsip::StatusCode::ServerInternalError)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Process messages between UAC and UAS
        let mut final_response_received = false;

        loop {
            if inv_tx.is_terminated() {
                break;
            }

            select! {
                // Messages from the callee (UAC)
                msg = inv_tx.receive() => {
                    debug!("UAC received message: {}", msg.as_ref().map(|m| m.to_string()).unwrap_or_default());

                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Response(mut resp) => {
                                // Remove first Via header (our Via)
                                header_pop!(resp.headers, rsip::Header::Via);

                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    final_response_received = true;

                                    // Store the dialog
                                    let session = Session {
                                        dialog_id: dialog_id.clone(),
                                        last_activity: Instant::now(),
                                        parties: (caller.clone(), callee.clone()),
                                    };

                                    self.inner.sessions.lock().unwrap().insert(dialog_id.clone(), session);
                                    info!("Session established: {}", dialog_id);
                                }

                                // Forward the response to the caller
                                tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                            }
                            _ => {}
                        }
                    }
                }

                // Messages from the caller (UAS)
                msg = tx.receive() => {
                    debug!("UAS received message: {}", msg.as_ref().map(|m| m.to_string()).unwrap_or_default());

                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Request(req) => match req.method {
                                rsip::Method::Ack => {
                                    if final_response_received {
                                        inv_tx.send_ack(req).await.map_err(|e| anyhow!(e))?;
                                    }
                                }
                                rsip::Method::Cancel => {
                                    inv_tx.send_cancel(req).await.map_err(|e| anyhow!(e))?;
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_reinvite(&self, tx: &mut Transaction) -> Result<()> {
        // Check if this is part of an existing dialog
        let dialog_id = match DialogId::try_from(&tx.original) {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to parse dialog ID: {}", e);
                return tx
                    .reply(rsip::StatusCode::BadRequest)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let session = {
            let sessions = self.inner.sessions.lock().unwrap();
            sessions.get(&dialog_id).cloned()
        };

        let (caller, callee) = match session {
            Some(s) => s.parties,
            None => {
                info!("Session not found for re-INVITE: {}", dialog_id);
                return tx
                    .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Update activity timestamp
        {
            if let Some(session) = self.inner.sessions.lock().unwrap().get_mut(&dialog_id) {
                session.last_activity = Instant::now();
            }
        }

        // Get callee's contact information
        let target = {
            let users = self.inner.users.lock().unwrap();
            users.get(&callee).cloned()
        };

        let target = match target {
            Some(u) => u,
            None => {
                info!("Target user not found for re-INVITE: {}", callee);
                return tx
                    .reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Forward the re-INVITE
        let mut inv_req = tx.original.clone();
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        inv_req.headers.push_front(via.into());

        let key = TransactionKey::from_request(&inv_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;

        info!("Forwarding re-INVITE: {} -> {}", caller, target.destination);

        let mut inv_tx = Transaction::new_client(key, inv_req, tx.endpoint_inner.clone(), None);
        inv_tx.destination = Some(target.destination);

        inv_tx.send().await.map_err(|e| anyhow!(e))?;

        // Handle responses and ACK similar to initial INVITE
        loop {
            if inv_tx.is_terminated() {
                break;
            }

            select! {
                msg = inv_tx.receive() => {
                    debug!("UAC re-INVITE received: {}", msg.as_ref().map(|m| m.to_string()).unwrap_or_default());

                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Response(mut resp) => {
                                header_pop!(resp.headers, rsip::Header::Via);
                                tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                            }
                            _ => {}
                        }
                    }
                }

                msg = tx.receive() => {
                    debug!("UAS re-INVITE received: {}", msg.as_ref().map(|m| m.to_string()).unwrap_or_default());

                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Request(req) => match req.method {
                                rsip::Method::Ack => {
                                    inv_tx.send_ack(req).await.map_err(|e| anyhow!(e))?;
                                }
                                rsip::Method::Cancel => {
                                    inv_tx.send_cancel(req).await.map_err(|e| anyhow!(e))?;
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_bye(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id = match DialogId::try_from(&tx.original) {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to parse dialog ID: {}", e);
                return tx
                    .reply(rsip::StatusCode::BadRequest)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let session = {
            let sessions = self.inner.sessions.lock().unwrap();
            sessions.get(&dialog_id).cloned()
        };

        let (caller, callee) = match session {
            Some(s) => s.parties,
            None => {
                info!("Session not found for BYE: {}", dialog_id);
                return tx
                    .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Get peer's contact information
        let target = {
            let users = self.inner.users.lock().unwrap();
            users.get(&callee).cloned()
        };

        let target = match target {
            Some(u) => u,
            None => {
                info!("Target user not found for BYE: {}", callee);
                return tx
                    .reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Forward the BYE request
        let mut bye_req = tx.original.clone();
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        bye_req.headers.push_front(via.into());

        let key = TransactionKey::from_request(&bye_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;

        info!("Forwarding BYE: {} -> {}", caller, target.destination);

        let mut bye_tx = Transaction::new_client(key, bye_req, tx.endpoint_inner.clone(), None);
        bye_tx.destination = Some(target.destination);

        bye_tx.send().await.map_err(|e| anyhow!(e))?;

        // Remove session
        {
            if self
                .inner
                .sessions
                .lock()
                .unwrap()
                .remove(&dialog_id)
                .is_some()
            {
                info!("Session terminated: {}", dialog_id);
            }
        }

        // Process response from peer
        while let Some(msg) = bye_tx.receive().await {
            match msg {
                rsip::message::SipMessage::Response(mut resp) => {
                    header_pop!(resp.headers, rsip::Header::Via);
                    tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                }
                _ => {
                    error!("Unexpected message type from BYE: {}", msg.to_string());
                }
            }
        }

        Ok(())
    }

    async fn handle_info(&self, tx: &mut Transaction) -> Result<()> {
        let dialog_id = match DialogId::try_from(&tx.original) {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to parse dialog ID: {}", e);
                return tx
                    .reply(rsip::StatusCode::BadRequest)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let session = {
            let sessions = self.inner.sessions.lock().unwrap();
            sessions.get(&dialog_id).cloned()
        };

        let (caller, callee) = match session {
            Some(s) => {
                // Update activity timestamp
                if let Some(session) = self.inner.sessions.lock().unwrap().get_mut(&dialog_id) {
                    session.last_activity = Instant::now();
                }
                s.parties
            }
            None => {
                info!("Session not found for INFO: {}", dialog_id);
                return tx
                    .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Get peer's contact information
        let target = {
            let users = self.inner.users.lock().unwrap();
            users.get(&callee).cloned()
        };

        let target = match target {
            Some(u) => u,
            None => {
                info!("Target user not found for INFO: {}", callee);
                return tx
                    .reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        // Forward the INFO request
        let mut info_req = tx.original.clone();
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        info_req.headers.push_front(via.into());

        let key = TransactionKey::from_request(&info_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;

        info!("Forwarding INFO: {} -> {}", caller, target.destination);

        let mut info_tx = Transaction::new_client(key, info_req, tx.endpoint_inner.clone(), None);
        info_tx.destination = Some(target.destination);

        info_tx.send().await.map_err(|e| anyhow!(e))?;

        // Process response from peer
        while let Some(msg) = info_tx.receive().await {
            match msg {
                rsip::message::SipMessage::Response(mut resp) => {
                    header_pop!(resp.headers, rsip::Header::Via);
                    tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                }
                _ => {
                    error!("Unexpected message type from INFO: {}", msg.to_string());
                }
            }
        }

        Ok(())
    }

    async fn handle_cancel(&self, tx: &mut Transaction) -> Result<()> {
        // CANCEL requests are handled by the transaction layer
        // We simply need to forward them to the other party
        tx.reply(rsip::StatusCode::OK)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    async fn handle_options(&self, tx: &mut Transaction) -> Result<()> {
        // OPTIONS can be used for monitoring dialog liveness
        // or as a general ping mechanism
        if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
            // If this is part of a dialog, update the activity timestamp
            if let Some(session) = self.inner.sessions.lock().unwrap().get_mut(&dialog_id) {
                session.last_activity = Instant::now();
            }
        }

        // Respond with 200 OK and our capabilities
        let headers = vec![
            rsip::Header::Allow(rsip::headers::Allow::new(
                "INVITE, ACK, CANCEL, OPTIONS, BYE, INFO",
            )),
            rsip::Header::Supported(rsip::headers::Supported::new("path")),
        ];

        tx.reply_with(rsip::StatusCode::OK, headers, None)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    async fn handle_ack(&self, tx: &mut Transaction) -> Result<()> {
        // Check if this ACK belongs to an existing dialog
        if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
            let session = {
                let sessions = self.inner.sessions.lock().unwrap();
                sessions.get(&dialog_id).cloned()
            };

            if let Some(session) = session {
                // Update activity timestamp
                {
                    if let Some(s) = self.inner.sessions.lock().unwrap().get_mut(&dialog_id) {
                        s.last_activity = Instant::now();
                    }
                }

                // Get callee's contact info
                let target = {
                    let users = self.inner.users.lock().unwrap();
                    users.get(&session.parties.1).cloned()
                };

                if let Some(target) = target {
                    // Forward the ACK
                    let mut ack_req = tx.original.clone();
                    let via = tx
                        .endpoint_inner
                        .get_via(None, None)
                        .map_err(|e| anyhow!(e))?;
                    ack_req.headers.push_front(via.into());

                    let key = TransactionKey::from_request(&ack_req, TransactionRole::Client)
                        .map_err(|e| anyhow!(e))?;

                    info!(
                        "Forwarding ACK: {} -> {}",
                        session.parties.0, target.destination
                    );

                    let mut ack_tx =
                        Transaction::new_client(key, ack_req, tx.endpoint_inner.clone(), None);
                    ack_tx.destination = Some(target.destination);

                    ack_tx.send().await.map_err(|e| anyhow!(e))?;
                    return Ok(());
                }
            }
            // If we get here, dialog exists but target not found or session not found
            info!(
                "Failed to forward ACK, dialog or user not found: {}",
                dialog_id
            );
        }

        // For non-dialog ACKs (e.g., ACK for error responses), nothing to do
        Ok(())
    }

    async fn check_dialog_liveness(&self, _dialog_id: &DialogId) -> Result<bool> {
        // This would typically send an OPTIONS request to check if both parties are still alive
        // For now we'll just return true as implementation placeholder

        // In a complete implementation, this would:
        // 1. Get both parties' contact info
        // 2. Send OPTIONS to both
        // 3. Wait for responses with timeout
        // 4. Return true if both respond, false otherwise

        Ok(true)
    }
}

#[async_trait]
impl ProxyModule for CallModule {
    fn name(&self) -> &str {
        "call"
    }

    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![
            rsip::Method::Invite,
            rsip::Method::Ack,
            rsip::Method::Bye,
            rsip::Method::Cancel,
            rsip::Method::Options,
            rsip::Method::Info,
            rsip::Method::Update,
            rsip::Method::Refer,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        match tx.original.method {
            rsip::Method::Invite => {
                // Check if this is an initial INVITE or re-INVITE
                let is_reinvite = if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
                    self.inner.sessions.lock().unwrap().contains_key(&dialog_id)
                } else {
                    false
                };

                // Handle the invite in this function since Transaction is not clonable
                if is_reinvite {
                    if let Err(e) = self.handle_reinvite(tx).await {
                        error!("Error handling re-INVITE: {}", e);
                    }
                } else {
                    if let Err(e) = self.handle_invite(tx).await {
                        error!("Error handling INVITE: {}", e);
                    }
                }

                Ok(ProxyAction::Abort) // We're handling this transaction
            }
            rsip::Method::Bye => {
                if let Err(e) = self.handle_bye(tx).await {
                    error!("Error handling BYE: {}", e);
                }

                Ok(ProxyAction::Abort)
            }
            rsip::Method::Info => {
                if let Err(e) = self.handle_info(tx).await {
                    error!("Error handling INFO: {}", e);
                }

                Ok(ProxyAction::Abort)
            }
            rsip::Method::Cancel => {
                if let Err(e) = self.handle_cancel(tx).await {
                    error!("Error handling CANCEL: {}", e);
                }

                Ok(ProxyAction::Abort)
            }
            rsip::Method::Options => {
                if let Err(e) = self.handle_options(tx).await {
                    error!("Error handling OPTIONS: {}", e);
                }

                Ok(ProxyAction::Abort)
            }
            rsip::Method::Ack => {
                if let Err(e) = self.handle_ack(tx).await {
                    error!("Error handling ACK: {}", e);
                }

                Ok(ProxyAction::Abort)
            }
            _ => Ok(ProxyAction::Continue),
        }
    }

    async fn on_transaction_end(&self, _tx: &mut Transaction) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;

    #[tokio::test]
    async fn test_call_module_basics() {
        let config = Arc::new(ProxyConfig::default());
        let inner = Arc::new(CallModuleInner {
            config: config.clone(),
            users: Arc::new(Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            session_timeout: Duration::from_secs(300),
            options_interval: Duration::from_secs(30),
        });

        let module = CallModule { inner };

        assert_eq!(module.name(), "call");
        assert!(module.allow_methods().contains(&rsip::Method::Invite));
        assert!(module.allow_methods().contains(&rsip::Method::Bye));
        assert!(module.allow_methods().contains(&rsip::Method::Info));
    }
}
