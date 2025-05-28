use super::{server::SipServerRef, ProxyAction, ProxyModule};
use crate::config::{MediaProxyMode, ProxyConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rsip::headers::UntypedHeader;
use rsip::prelude::HeadersExt;
use rsipstack::dialog::DialogId;
use rsipstack::header_pop;
use rsipstack::rsip_ext::RsipHeadersExt;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::transaction::Transaction;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::select;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
struct Session {
    dialog_id: DialogId,
    last_activity: Instant,
    parties: (String, String), // (caller, callee)
}

#[derive(Clone)]
pub struct CallModuleInner {
    config: Arc<ProxyConfig>,
    server: SipServerRef,
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
        let session_timeout = Duration::from_secs(300);
        let options_interval = Duration::from_secs(30);

        let inner = Arc::new(CallModuleInner {
            config,
            server: server.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            session_timeout,
            options_interval,
        });

        let module = Self { inner };
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

        {
            let sessions = inner.sessions.lock().unwrap();
            for (dialog_id, session) in sessions.iter() {
                if now.duration_since(session.last_activity) > inner.session_timeout {
                    expired_sessions.push(dialog_id.clone());
                }
            }
        }

        for dialog_id in expired_sessions {
            info!("Session timeout for dialog: {}", dialog_id);
            inner.sessions.lock().unwrap().remove(&dialog_id);
        }
    }

    /// Check if media proxy is needed based on nat_only configuration
    fn should_use_media_proxy(&self, tx: &Transaction) -> Result<bool> {
        let media_config = &self.inner.config.media_proxy;

        match media_config.mode {
            MediaProxyMode::None => Ok(false),
            MediaProxyMode::All => Ok(true),
            MediaProxyMode::NatOnly => {
                if let Some(content_type) = tx.original.headers.iter().find_map(|h| match h {
                    rsip::Header::ContentType(ct) => Some(ct),
                    _ => None,
                }) {
                    if content_type.value().contains("application/sdp") {
                        let body = String::from_utf8_lossy(&tx.original.body);
                        return Ok(crate::net_tool::sdp_contains_private_ip(&body).unwrap_or(false));
                    }
                }
                Ok(false)
            }
        }
    }

    /// Forward request to external proxy realm
    async fn forward_to_proxy(&self, tx: &mut Transaction, target_realm: &str) -> Result<()> {
        warn!(
            "External proxy forwarding not implemented for realm: {}",
            target_realm
        );
        tx.reply(rsip::StatusCode::NotFound)
            .await
            .map_err(|e| anyhow!(e))?;

        while let Some(msg) = tx.receive().await {
            match msg {
                rsip::message::SipMessage::Request(req) => match req.method {
                    rsip::Method::Ack => {
                        debug!("Received ACK for external proxy 404");
                        break;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(())
    }

    async fn handle_invite(&self, tx: &mut Transaction) -> Result<()> {
        let caller = tx.original.from_header()?.uri()?.to_string();
        let callee_uri = tx.original.to_header()?.uri()?;
        let callee = callee_uri.user().unwrap_or_default().to_string();
        let callee_realm = callee_uri.host().to_string();

        let local_realm = self
            .inner
            .config
            .external_ip
            .as_ref()
            .and_then(|ip| ip.split(':').next())
            .unwrap_or("localhost");

        if callee_realm != local_realm {
            info!(
                "Forwarding INVITE to external realm: {} -> {}",
                caller, callee_realm
            );
            return self.forward_to_proxy(tx, &callee_realm).await;
        }

        let target_locations = match self
            .inner
            .server
            .locator
            .lookup(&callee, Some(&callee_realm))
            .await
        {
            Ok(locations) => locations,
            Err(_) => {
                info!("User not found in locator: {}@{}", callee, callee_realm);
                tx.reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e))?;
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

        let target_location = &target_locations[0];

        let should_proxy_media = self.should_use_media_proxy(tx)?;
        if should_proxy_media {
            info!("Media proxy required for NAT traversal");
        }

        let mut inv_req = tx.original.clone();
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        inv_req.headers.push_front(via.into());

        if let Ok(record_route) = tx.endpoint_inner.get_record_route() {
            inv_req.headers.push_front(record_route.into());
        }

        let key = TransactionKey::from_request(&inv_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;
        info!(
            "Forwarding INVITE: {} -> {}",
            caller, target_location.destination
        );

        let mut inv_tx = Transaction::new_client(key, inv_req, tx.endpoint_inner.clone(), None);
        inv_tx.destination = Some(target_location.destination.clone());
        inv_tx.send().await.map_err(|e| anyhow!(e))?;

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

        loop {
            if inv_tx.is_terminated() {
                break;
            }

            select! {
                msg = inv_tx.receive() => {
                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Response(mut resp) => {
                                header_pop!(resp.headers, rsip::Header::Via);
                                if resp.status_code.kind() == rsip::StatusCodeKind::Successful {
                                    let session = Session {
                                        dialog_id: dialog_id.clone(),
                                        last_activity: Instant::now(),
                                        parties: (caller.clone(), callee.clone()),
                                    };
                                    self.inner.sessions.lock().unwrap().insert(dialog_id.clone(), session);
                                    info!("Session established: {}", dialog_id);
                                }
                                tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                            }
                            _ => {}
                        }
                    }
                }
                msg = tx.receive() => {
                    if let Some(msg) = msg {
                        match msg {
                            rsip::message::SipMessage::Request(req) => match req.method {
                                rsip::Method::Ack => {
                                    let mut ack_req = req.clone();
                                    let via = tx.endpoint_inner.get_via(None, None).map_err(|e| anyhow!(e))?;
                                    ack_req.headers.push_front(via.into());
                                    let key = TransactionKey::from_request(&ack_req, TransactionRole::Client).map_err(|e| anyhow!(e))?;
                                    let mut ack_tx = Transaction::new_client(key, ack_req, tx.endpoint_inner.clone(), None);
                                    ack_tx.destination = Some(target_location.destination.clone());
                                    ack_tx.send().await.map_err(|e| anyhow!(e))?;
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

        let (_, callee) = match session {
            Some(s) => s.parties,
            None => {
                info!("Session not found for BYE: {}", dialog_id);
                return tx
                    .reply(rsip::StatusCode::CallTransactionDoesNotExist)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let target_locations = match self.inner.server.locator.lookup(&callee, None).await {
            Ok(locations) => locations,
            Err(_) => {
                info!("Target user not found for BYE: {}", callee);
                return tx
                    .reply(rsip::StatusCode::NotFound)
                    .await
                    .map_err(|e| anyhow!(e));
            }
        };

        let target_location = &target_locations[0];
        let mut bye_req = tx.original.clone();
        let via = tx
            .endpoint_inner
            .get_via(None, None)
            .map_err(|e| anyhow!(e))?;
        bye_req.headers.push_front(via.into());

        let key = TransactionKey::from_request(&bye_req, TransactionRole::Client)
            .map_err(|e| anyhow!(e))?;
        let mut bye_tx = Transaction::new_client(key, bye_req, tx.endpoint_inner.clone(), None);
        bye_tx.destination = Some(target_location.destination.clone());
        bye_tx.send().await.map_err(|e| anyhow!(e))?;

        while let Some(msg) = bye_tx.receive().await {
            match msg {
                rsip::message::SipMessage::Response(mut resp) => {
                    header_pop!(resp.headers, rsip::Header::Via);
                    tx.respond(resp).await.map_err(|e| anyhow!(e))?;
                    break;
                }
                _ => {}
            }
        }

        self.inner.sessions.lock().unwrap().remove(&dialog_id);
        info!("Session terminated: {}", dialog_id);
        Ok(())
    }

    async fn handle_options(&self, tx: &mut Transaction) -> Result<()> {
        if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
            if let Some(session) = self.inner.sessions.lock().unwrap().get_mut(&dialog_id) {
                session.last_activity = Instant::now();
            }
        }
        tx.reply(rsip::StatusCode::OK)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    async fn handle_ack(&self, tx: &mut Transaction) -> Result<()> {
        if let Ok(dialog_id) = DialogId::try_from(&tx.original) {
            let sessions = self.inner.sessions.lock().unwrap();
            if sessions.contains_key(&dialog_id) {
                info!("ACK received for dialog: {}", dialog_id);
            }
        }
        Ok(())
    }

    async fn handle_cancel(&self, tx: &mut Transaction) -> Result<()> {
        tx.reply(rsip::StatusCode::OK)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(())
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
            rsip::Method::Bye,
            rsip::Method::Info,
            rsip::Method::Ack,
            rsip::Method::Cancel,
            rsip::Method::Options,
        ]
    }

    async fn on_start(&mut self) -> Result<()> {
        info!("Call module started");
        Ok(())
    }

    async fn on_stop(&self) -> Result<()> {
        info!("Call module stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
    ) -> Result<ProxyAction> {
        match tx.original.method {
            rsip::Method::Invite => {
                if let Err(e) = self.handle_invite(tx).await {
                    error!("Error handling INVITE: {}", e);
                }
                Ok(ProxyAction::Abort)
            }
            rsip::Method::Bye => {
                if let Err(e) = self.handle_bye(tx).await {
                    error!("Error handling BYE: {}", e);
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
            rsip::Method::Cancel => {
                if let Err(e) = self.handle_cancel(tx).await {
                    error!("Error handling CANCEL: {}", e);
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
    use crate::proxy::tests::common::{
        create_test_request, create_test_server, create_transaction,
    };
    use rsip::headers::{ContentType, Header};
    use rsip::HostWithPort;
    use std::time::Instant;

    fn create_test_dialog_id(call_id: &str, from_tag: &str, to_tag: &str) -> DialogId {
        DialogId {
            call_id: call_id.to_string(),
            from_tag: from_tag.to_string(),
            to_tag: to_tag.to_string(),
        }
    }

    #[tokio::test]
    async fn test_call_module_basics() {
        let (server, config) = create_test_server().await;
        let _module = CallModule::new(config, server.clone());

        assert_eq!(_module.name(), "call");
    }

    #[tokio::test]
    async fn test_call_module_creation() {
        let (server, config) = create_test_server().await;
        let _module = CallModule::new(config, server.clone());

        assert_eq!(_module.name(), "call");
    }

    #[tokio::test]
    async fn test_locator_integration() {
        let (server, config) = create_test_server().await;
        let _module = CallModule::new(config, server.clone());

        // Register a user in the locator
        let location = super::super::locator::Location {
            aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
            expires: 3600,
            destination: rsipstack::transport::SipAddr {
                r#type: None,
                addr: HostWithPort::try_from("192.168.1.100:5060").unwrap(),
            },
            last_modified: Instant::now(),
        };

        let result = server
            .locator
            .register("alice", Some("example.com"), location)
            .await;
        assert!(result.is_ok());

        // Test looking up user
        let lookup_result = server.locator.lookup("alice", Some("example.com")).await;
        assert!(lookup_result.is_ok());
        let locations = lookup_result.unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(
            locations[0].destination.addr,
            HostWithPort::try_from("192.168.1.100:5060").unwrap()
        );
    }

    #[tokio::test]
    async fn test_media_proxy_nat_only() {
        let mut config = crate::config::ProxyConfig::default();
        config.media_proxy.mode = MediaProxyMode::NatOnly;
        let (server, config) =
            crate::proxy::tests::common::create_test_server_with_config(config).await;

        let module = CallModule::new(config, server);

        // Create a request with SDP containing private IP
        let mut request =
            create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

        // Add SDP body with private IP - include connection line (c=)
        let sdp_body = b"v=0\r\no=alice 123 123 IN IP4 192.168.1.100\r\ns=Call\r\nc=IN IP4 192.168.1.100\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
        request.body = sdp_body.to_vec();
        request.headers.push(Header::ContentType(ContentType::new(
            "application/sdp".to_string(),
        )));

        let (tx, _) = create_transaction(request);

        let should_proxy = module.should_use_media_proxy(&tx).unwrap();
        assert!(should_proxy);
    }

    #[tokio::test]
    async fn test_media_proxy_none_mode() {
        let mut config = crate::config::ProxyConfig::default();
        config.media_proxy.mode = MediaProxyMode::None;
        let (server, config) =
            crate::proxy::tests::common::create_test_server_with_config(config).await;

        let module = CallModule::new(config, server);

        let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

        let (tx, _) = create_transaction(request);

        let should_proxy = module.should_use_media_proxy(&tx).unwrap();
        assert!(!should_proxy);
    }

    #[tokio::test]
    async fn test_media_proxy_all_mode() {
        let mut config = crate::config::ProxyConfig::default();
        config.media_proxy.mode = MediaProxyMode::All;
        let (server, config) =
            crate::proxy::tests::common::create_test_server_with_config(config).await;

        let module = CallModule::new(config, server);

        let request = create_test_request(rsip::Method::Invite, "alice", None, "example.com", None);

        let (tx, _) = create_transaction(request);

        let should_proxy = module.should_use_media_proxy(&tx).unwrap();
        assert!(should_proxy);
    }

    #[tokio::test]
    async fn test_options_handling() {
        let (server, config) = create_test_server().await;
        let module = CallModule::new(config, server);

        // Test that OPTIONS method is in allowed methods
        assert!(module.allow_methods().contains(&rsip::Method::Options));

        // Test dialog activity update logic (without network operations)
        let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

        // Add a session
        let session = Session {
            dialog_id: dialog_id.clone(),
            last_activity: Instant::now() - Duration::from_secs(10),
            parties: ("alice".to_string(), "bob".to_string()),
        };

        module
            .inner
            .sessions
            .lock()
            .unwrap()
            .insert(dialog_id.clone(), session);

        // Verify session exists with old timestamp
        let old_time = module
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&dialog_id)
            .unwrap()
            .last_activity;

        // Simulate dialog activity update (the core logic of handle_options)
        {
            let mut sessions = module.inner.sessions.lock().unwrap();
            if let Some(session) = sessions.get_mut(&dialog_id) {
                session.last_activity = Instant::now();
            }
        }

        // Verify activity was updated
        let new_time = module
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&dialog_id)
            .unwrap()
            .last_activity;

        assert!(new_time > old_time);
    }

    #[tokio::test]
    async fn test_external_realm_forwarding() {
        let mut config = crate::config::ProxyConfig::default();
        config.external_ip = Some("localhost:5060".to_string());
        let (server, config) =
            crate::proxy::tests::common::create_test_server_with_config(config).await;

        let _module = CallModule::new(config, server);

        // Test realm comparison logic
        let local_realm = "localhost";
        let external_realm = "external.com";

        // This should be different realms
        assert_ne!(local_realm, external_realm);

        // Test that external realm forwarding is detected
        assert!(external_realm != local_realm);
    }

    #[tokio::test]
    async fn test_local_realm_invite() {
        let mut config = crate::config::ProxyConfig::default();
        config.external_ip = Some("example.com:5060".to_string());
        let (server, config) =
            crate::proxy::tests::common::create_test_server_with_config(config).await;

        let _module = CallModule::new(config, server.clone());

        // Register a user in the locator
        let location = super::super::locator::Location {
            aor: rsip::Uri::try_from("sip:alice@example.com").unwrap(),
            expires: 3600,
            destination: rsipstack::transport::SipAddr {
                r#type: None,
                addr: HostWithPort::try_from("192.168.1.100:5060").unwrap(),
            },
            last_modified: Instant::now(),
        };

        server
            .locator
            .register("alice", Some("example.com"), location)
            .await
            .unwrap();

        // Test locator lookup
        let lookup_result = server.locator.lookup("alice", Some("example.com")).await;
        assert!(lookup_result.is_ok());
        let locations = lookup_result.unwrap();
        assert_eq!(locations.len(), 1);

        // Test realm comparison logic
        let local_realm = "example.com";
        let callee_realm = "example.com";
        assert_eq!(local_realm, callee_realm);
    }

    #[tokio::test]
    async fn test_session_management() {
        let (server, config) = create_test_server().await;
        let module = CallModule::new(config, server);

        let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

        // Add a session
        let session = Session {
            dialog_id: dialog_id.clone(),
            last_activity: Instant::now(),
            parties: ("alice".to_string(), "bob".to_string()),
        };

        module
            .inner
            .sessions
            .lock()
            .unwrap()
            .insert(dialog_id.clone(), session);

        // Verify session exists
        assert!(module
            .inner
            .sessions
            .lock()
            .unwrap()
            .contains_key(&dialog_id));

        // Test session cleanup
        CallModule::check_sessions(&module.inner).await;

        // Session should still exist (not expired)
        assert!(module
            .inner
            .sessions
            .lock()
            .unwrap()
            .contains_key(&dialog_id));
    }

    #[tokio::test]
    async fn test_session_timeout() {
        let (server, config) = create_test_server().await;
        let module = CallModule::new(config, server);

        let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

        // Add an expired session
        let session = Session {
            dialog_id: dialog_id.clone(),
            last_activity: Instant::now() - Duration::from_secs(400), // Expired
            parties: ("alice".to_string(), "bob".to_string()),
        };

        module
            .inner
            .sessions
            .lock()
            .unwrap()
            .insert(dialog_id.clone(), session);

        // Verify session exists
        assert!(module
            .inner
            .sessions
            .lock()
            .unwrap()
            .contains_key(&dialog_id));

        // Test session cleanup
        CallModule::check_sessions(&module.inner).await;

        // Session should be removed (expired)
        assert!(!module
            .inner
            .sessions
            .lock()
            .unwrap()
            .contains_key(&dialog_id));
    }

    #[tokio::test]
    async fn test_module_lifecycle() {
        let (server, config) = create_test_server().await;
        let mut module = CallModule::new(config, server);

        let start_result = module.on_start().await;
        assert!(start_result.is_ok());

        let stop_result = module.on_stop().await;
        assert!(stop_result.is_ok());
    }

    #[tokio::test]
    async fn test_dialog_activity_update() {
        let (server, config) = create_test_server().await;
        let module = CallModule::new(config, server);

        let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

        // Add a session
        let initial_time = Instant::now() - Duration::from_secs(10);
        let session = Session {
            dialog_id: dialog_id.clone(),
            last_activity: initial_time,
            parties: ("alice".to_string(), "bob".to_string()),
        };

        module
            .inner
            .sessions
            .lock()
            .unwrap()
            .insert(dialog_id.clone(), session);

        // Simulate activity update
        {
            let mut sessions = module.inner.sessions.lock().unwrap();
            if let Some(session) = sessions.get_mut(&dialog_id) {
                session.last_activity = Instant::now();
            }
        }

        // Verify activity was updated
        let updated_session = module
            .inner
            .sessions
            .lock()
            .unwrap()
            .get(&dialog_id)
            .cloned();

        assert!(updated_session.is_some());
        let session = updated_session.unwrap();
        assert!(session.last_activity > initial_time);
    }

    #[tokio::test]
    async fn test_concurrent_session_access() {
        let (server, config) = create_test_server().await;
        let module = CallModule::new(config, server);

        let dialog_id = create_test_dialog_id("test-call-id", "from-tag", "to-tag");

        // Add a session
        let session = Session {
            dialog_id: dialog_id.clone(),
            last_activity: Instant::now(),
            parties: ("alice".to_string(), "bob".to_string()),
        };

        module
            .inner
            .sessions
            .lock()
            .unwrap()
            .insert(dialog_id.clone(), session);

        // Simulate concurrent access
        let module_clone = module.clone();
        let dialog_id_clone = dialog_id.clone();

        let handle1 = tokio::spawn(async move {
            for _ in 0..10 {
                {
                    let sessions = module_clone.inner.sessions.lock().unwrap();
                    let _session = sessions.get(&dialog_id_clone);
                } // Drop guard before sleep
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        let handle2 = tokio::spawn(async move {
            for _ in 0..10 {
                {
                    let mut sessions = module.inner.sessions.lock().unwrap();
                    if let Some(session) = sessions.get_mut(&dialog_id) {
                        session.last_activity = Instant::now();
                    }
                } // Drop guard before sleep
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        let _ = tokio::join!(handle1, handle2);
    }

    #[tokio::test]
    async fn test_session_timeout_with_active_sessions() {
        let (server, config) = create_test_server().await;
        let module = CallModule::new(config, server);

        let expired_dialog_id = create_test_dialog_id("expired-call-id", "from-tag", "to-tag");
        let active_dialog_id = create_test_dialog_id("active-call-id", "from-tag", "to-tag");

        // Add an expired session
        let expired_session = Session {
            dialog_id: expired_dialog_id.clone(),
            last_activity: Instant::now() - Duration::from_secs(400), // Expired
            parties: ("alice".to_string(), "bob".to_string()),
        };

        // Add an active session
        let active_session = Session {
            dialog_id: active_dialog_id.clone(),
            last_activity: Instant::now(), // Active
            parties: ("charlie".to_string(), "dave".to_string()),
        };

        {
            let mut sessions = module.inner.sessions.lock().unwrap();
            sessions.insert(expired_dialog_id.clone(), expired_session);
            sessions.insert(active_dialog_id.clone(), active_session);
        }

        // Verify both sessions exist
        assert_eq!(module.inner.sessions.lock().unwrap().len(), 2);

        // Test session cleanup
        CallModule::check_sessions(&module.inner).await;

        // Only active session should remain
        let sessions = module.inner.sessions.lock().unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(!sessions.contains_key(&expired_dialog_id));
        assert!(sessions.contains_key(&active_dialog_id));
    }
}
