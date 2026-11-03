//! End-to-end RFC 8599 flows over the real SIP stack: a phone registers with
//! push parameters, the app is killed (its flow closes), and an incoming call
//! — direct, through a route or a queue — wakes it through a mock push
//! provider.

use super::test_helpers;
use super::test_ua::{TestUa, TestUaConfig, create_test_sdp, create_test_sdp_answer};
use crate::config::PushConfig;
use crate::proxy::{
    locator::MemoryLocator,
    push::{
        FlowState, PushError, PushKind, PushNotification, PushParams, PushProvider, PushRegistry,
        PushService,
    },
    server::{SipServerBuilder, SipServerRef},
    user::MemoryUserBackend,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::Dialog;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::registration::Registration;
use rsipstack::sip::prelude::HeadersExt;
use rsipstack::sip::uri::{OtherParam, OtherParamValue};
use rsipstack::sip::{Header, Method, Param, Request, Response, StatusCode, Uri};
use rsipstack::transaction::EndpointBuilder;
use rsipstack::transport::TransportLayer;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

const INSTANCE: &str = "7c5d2a3e-0000-4000-8000-0000000000b0";

/// Push provider that records pushes instead of sending them.
struct MockPushProvider {
    sent: mpsc::UnboundedSender<(PushParams, PushNotification)>,
}

#[async_trait]
impl PushProvider for MockPushProvider {
    fn name(&self) -> &str {
        "fcm"
    }

    fn accepts(&self, _params: &PushParams) -> bool {
        true
    }

    async fn send(
        &self,
        params: &PushParams,
        notification: &PushNotification,
    ) -> Result<(), PushError> {
        let _ = self.sent.send((params.clone(), notification.clone()));
        Ok(())
    }
}

struct PushTestServer {
    server: SipServerRef,
    udp_addr: SocketAddr,
    tcp_addr: SocketAddr,
    pushes: mpsc::UnboundedReceiver<(PushParams, PushNotification)>,
    cancel: CancellationToken,
}

impl PushTestServer {
    async fn start(push_config: PushConfig) -> Result<Self> {
        Self::start_with(push_config, |_| {}).await
    }

    async fn start_with(
        push_config: PushConfig,
        customize: impl FnOnce(&mut crate::config::ProxyConfig),
    ) -> Result<Self> {
        let _ = tracing_subscriber::fmt::try_init();
        let udp_port = portpicker::pick_unused_port().ok_or_else(|| anyhow!("no udp port"))?;
        let tcp_port = portpicker::pick_unused_port().ok_or_else(|| anyhow!("no tcp port"))?;
        let mut proxy_config = test_helpers::test_proxy_config(udp_port);
        proxy_config.tcp_port = Some(tcp_port);
        proxy_config.ensure_user = Some(false);
        proxy_config.enable_latching = false;
        proxy_config.push = Some(push_config.clone());
        customize(&mut proxy_config);

        let user_backend = MemoryUserBackend::new(None);
        for user in test_helpers::standard_test_users() {
            user_backend.create_user(user).await?;
        }

        let (sent, pushes) = mpsc::unbounded_channel();
        let push = Arc::new(PushService::new(
            push_config,
            vec![Arc::new(MockPushProvider { sent })],
            PushRegistry::new(),
        ));

        let cancel = CancellationToken::new();
        let builder = test_helpers::register_standard_modules(
            SipServerBuilder::new(Arc::new(proxy_config))
                .with_user_backend(Box::new(user_backend))
                .with_locator(Box::new(MemoryLocator::new()))
                .with_cancel_token(cancel.clone())
                .with_push_service(push),
        );
        let server = Arc::new(builder.build().await?);
        let server_ref = server.get_inner();
        let serve_cancel = cancel.clone();
        crate::utils::spawn(async move {
            tokio::select! {
                _ = serve_cancel.cancelled() => {}
                _ = server.serve() => {}
            }
        });
        sleep(Duration::from_millis(200)).await;

        Ok(Self {
            server: server_ref,
            udp_addr: format!("127.0.0.1:{udp_port}").parse()?,
            tcp_addr: format!("127.0.0.1:{tcp_port}").parse()?,
            pushes,
            cancel,
        })
    }

    async fn caller(&self) -> Result<TestUa> {
        let mut ua = TestUa::new(TestUaConfig {
            webrtc: false,
            username: "alice".to_string(),
            password: "password123".to_string(),
            realm: "127.0.0.1".to_string(),
            local_port: portpicker::pick_unused_port().ok_or_else(|| anyhow!("no port"))?,
            proxy_addr: self.udp_addr,
            extra_invite_headers: Vec::new(),
        });
        ua.start().await?;
        ua.register().await?;
        Ok(ua)
    }

    async fn bob_bindings(&self) -> usize {
        let aor = Uri::try_from("sip:bob@127.0.0.1").unwrap();
        self.server
            .locator
            .lookup(&aor)
            .await
            .map(|locs| locs.len())
            .unwrap_or(0)
    }

    /// Wait until bob has `expected` bindings, all on closed flows: his app
    /// is gone but stays registered, reachable through push.
    async fn wait_bob_asleep(&self, expected: usize) -> Result<()> {
        let push = self.server.push.clone().expect("push service");
        let aor = Uri::try_from("sip:bob@127.0.0.1").unwrap();
        for _ in 0..50 {
            let bindings = self.server.locator.lookup(&aor).await.unwrap_or_default();
            if bindings.len() == expected
                && bindings.iter().all(|binding| {
                    binding
                        .destination
                        .as_ref()
                        .is_some_and(|d| push.flow_state(d) == FlowState::Down)
                })
            {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow!("bob is not asleep with {expected} binding(s)"))
    }

    async fn wait_bob_bindings(&self, expected: usize) -> Result<()> {
        for _ in 0..50 {
            if self.bob_bindings().await == expected {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow!(
            "bob has {} bindings, expected {expected}",
            self.bob_bindings().await
        ))
    }

    async fn next_push(&mut self) -> Result<(PushParams, PushNotification)> {
        timeout(Duration::from_secs(5), self.pushes.recv())
            .await
            .map_err(|_| anyhow!("no push sent"))?
            .ok_or_else(|| anyhow!("push channel closed"))
    }
}

impl Drop for PushTestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A mobile app instance: one SIP endpoint whose only flow is the TCP
/// connection it registers on (or its UDP socket). Killing it closes that
/// flow; starting it again opens a new one, like Linphone being relaunched by
/// a push.
struct Phone {
    cancel: CancellationToken,
    dialog_layer: Arc<DialogLayer>,
    invites: mpsc::UnboundedReceiver<(DialogId, Request)>,
    /// Local UDP port when registering over UDP instead of TCP.
    udp_port: Option<u16>,
    _states: rsipstack::dialog::dialog::DialogStateReceiver,
}

impl Phone {
    async fn start() -> Result<Self> {
        Self::start_on(None).await
    }

    async fn start_udp() -> Result<Self> {
        Self::start_on(Some(
            portpicker::pick_unused_port().ok_or_else(|| anyhow!("no port"))?,
        ))
        .await
    }

    async fn start_on(udp_port: Option<u16>) -> Result<Self> {
        let cancel = CancellationToken::new();
        let transport_layer = TransportLayer::new(cancel.clone());
        if let Some(port) = udp_port {
            let connection = rsipstack::transport::udp::UdpConnection::create_connection(
                format!("127.0.0.1:{port}").parse()?,
                None,
                Some(cancel.clone()),
            )
            .await
            .map_err(|e| anyhow!("udp bind: {e:?}"))?;
            transport_layer.add_transport(connection.into());
        }
        let endpoint = EndpointBuilder::new()
            .with_cancel_token(cancel.clone())
            .with_transport_layer(transport_layer)
            .build();
        let mut incoming = endpoint.incoming_transactions()?;
        let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
        let (state_tx, states) = dialog_layer.new_dialog_state_channel();
        let (invite_tx, invites) = mpsc::unbounded_channel();

        let serve_cancel = cancel.clone();
        crate::utils::spawn(async move {
            tokio::select! {
                _ = endpoint.serve() => {}
                _ = serve_cancel.cancelled() => {}
            }
        });

        let layer = dialog_layer.clone();
        let incoming_cancel = cancel.clone();
        crate::utils::spawn(async move {
            loop {
                let mut tx = tokio::select! {
                    tx = incoming.recv() => match tx {
                        Some(tx) => tx,
                        None => break,
                    },
                    _ = incoming_cancel.cancelled() => break,
                };
                let in_dialog = tx
                    .original
                    .to_header()
                    .ok()
                    .and_then(|to| to.tag().ok().flatten())
                    .is_some();
                if in_dialog && let Some(mut dialog) = layer.match_dialog(&tx) {
                    crate::utils::spawn(async move {
                        dialog.handle(&mut tx).await.ok();
                    });
                    continue;
                }
                match tx.original.method {
                    Method::Invite | Method::Ack => {
                        let contact = Uri::try_from("sip:bob@127.0.0.1").unwrap();
                        let Ok(mut dialog) = layer.get_or_create_server_invite(
                            &tx,
                            state_tx.clone(),
                            None,
                            Some(contact),
                        ) else {
                            continue;
                        };
                        if tx.original.method == Method::Invite {
                            let _ = invite_tx.send((dialog.id(), tx.original.clone()));
                        }
                        crate::utils::spawn(async move {
                            dialog.handle(&mut tx).await.ok();
                        });
                    }
                    _ => {
                        tx.reply(StatusCode::OK).await.ok();
                    }
                }
            }
        });

        Ok(Self {
            cancel,
            dialog_layer,
            invites,
            udp_port,
            _states: states,
        })
    }

    /// REGISTER bob over TCP with `contact_params` on the Contact URI.
    async fn register(
        &self,
        server: SocketAddr,
        contact_params: &str,
        expires: u32,
    ) -> Result<Response> {
        self.register_instance(server, INSTANCE, contact_params, expires)
            .await
    }

    async fn register_instance(
        &self,
        server: SocketAddr,
        instance: &str,
        contact_params: &str,
        expires: u32,
    ) -> Result<Response> {
        let mut registration = Registration::new(
            self.dialog_layer.endpoint.clone(),
            Some(Credential {
                username: "bob".to_string(),
                auth_username: None,
                password: "password456".to_string(),
                realm: Some("127.0.0.1".to_string()),
            }),
        );
        let (contact, server) = match self.udp_port {
            Some(port) => (
                format!("sip:bob@127.0.0.1:{port}{contact_params}"),
                format!("sip:127.0.0.1:{}", server.port()),
            ),
            None => (
                format!("sip:bob@127.0.0.1:5999;transport=tcp{contact_params}"),
                format!("sip:127.0.0.1:{};transport=tcp", server.port()),
            ),
        };
        registration.contact = Some(rsipstack::sip::typed::Contact {
            display_name: None,
            uri: Uri::try_from(contact.as_str())?,
            params: vec![Param::Other(
                OtherParam::new("+sip.instance"),
                Some(OtherParamValue::new(format!("\"<urn:uuid:{instance}>\""))),
            )],
        });
        let server = Uri::try_from(server.as_str())?;
        timeout(
            Duration::from_secs(5),
            registration.register(server, Some(expires)),
        )
        .await
        .map_err(|_| anyhow!("REGISTER timed out"))?
        .map_err(|e| anyhow!("REGISTER failed: {e:?}"))
    }

    async fn launch(server: SocketAddr, prid: &str) -> Result<Self> {
        Self::launch_instance(server, INSTANCE, prid).await
    }

    /// Start the app over UDP and register with push parameters.
    async fn launch_udp(server: SocketAddr, prid: &str) -> Result<Self> {
        let phone = Self::start_udp().await?;
        phone.register_push(server, INSTANCE, prid).await?;
        Ok(phone)
    }

    /// Start the app and register with push parameters.
    async fn launch_instance(server: SocketAddr, instance: &str, prid: &str) -> Result<Self> {
        let phone = Self::start().await?;
        phone.register_push(server, instance, prid).await?;
        Ok(phone)
    }

    async fn register_push(&self, server: SocketAddr, instance: &str, prid: &str) -> Result<()> {
        let response = self
            .register_instance(
                server,
                instance,
                &format!(";pn-provider=fcm;pn-prid={prid};pn-param=test-project"),
                3600,
            )
            .await?;
        if response.status_code != StatusCode::OK {
            return Err(anyhow!("REGISTER rejected: {}", response.status_code));
        }
        Ok(())
    }

    fn kill(&self) {
        self.cancel.cancel();
    }

    /// Ring and answer the next incoming INVITE; returns it.
    async fn answer_next_invite(&mut self) -> Result<Request> {
        let (dialog_id, invite) = timeout(Duration::from_secs(10), self.invites.recv())
            .await
            .map_err(|_| anyhow!("phone received no INVITE"))?
            .ok_or_else(|| anyhow!("phone stopped"))?;
        let Some(Dialog::Invite(dialog)) = self.dialog_layer.get_dialog(&dialog_id) else {
            return Err(anyhow!("no dialog for incoming INVITE"));
        };
        dialog
            .ringing(None, None)
            .map_err(|e| anyhow!("ringing: {e:?}"))?;
        let offer = String::from_utf8_lossy(invite.body()).to_string();
        let answer = create_test_sdp_answer(&offer, "127.0.0.1", 40000);
        dialog
            .accept(
                Some(vec![Header::ContentType("application/sdp".into())]),
                Some(answer.into_bytes()),
            )
            .map_err(|e| anyhow!("accept: {e:?}"))?;
        Ok(invite)
    }
}

impl Drop for Phone {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn header_values(response: &Response, name: &str) -> Vec<String> {
    response
        .headers
        .iter()
        .filter_map(|header| {
            let text = header.to_string();
            let (key, value) = text.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
        .collect()
}

fn call_id(request: &Request) -> String {
    request
        .call_id_header()
        .map(|h| h.value().to_string())
        .unwrap_or_default()
}

fn caller_sdp() -> String {
    create_test_sdp("127.0.0.1", 41000, false)
}

/// The app is killed (its connection closes), a call arrives: the phone is
/// pushed, relaunches, re-registers on a new connection and gets the INVITE.
#[tokio::test]
async fn sleeping_device_is_woken_by_push() -> Result<()> {
    let mut server = PushTestServer::start(PushConfig {
        wakeup_timeout_secs: 10,
        ..Default::default()
    })
    .await?;

    let phone = Phone::start().await?;
    let response = phone
        .register(
            server.tcp_addr,
            ";pn-provider=fcm;pn-prid=token-1;pn-param=test-project;pn-silent=1",
            604_800,
        )
        .await?;
    assert_eq!(response.status_code, StatusCode::OK);
    assert_eq!(
        header_values(&response, "Feature-Caps"),
        vec!["*;+sip.pns=\"fcm\"".to_string()]
    );
    let push = server.server.push.clone().expect("push service");
    let devices = push.registry().devices("bob@localhost", 0);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].params.prid, "token-1");

    // The app is gone; its binding stays, reachable through push.
    phone.kill();
    server.wait_bob_asleep(1).await?;

    let alice = server.caller().await?;
    let call = tokio::spawn(async move {
        let result = alice.make_call("bob", Some(caller_sdp())).await;
        (alice, result)
    });

    let (params, notification) = server.next_push().await?;
    assert_eq!(params.prid, "token-1");
    assert_eq!(notification.kind, PushKind::Call);
    let pushed_call_id = notification.call_id.clone().expect("call-id in push");
    assert!(notification.caller_uri.is_none(), "caller info is opt-in");

    let mut relaunched = Phone::launch(server.tcp_addr, "token-1").await?;
    let invite = relaunched.answer_next_invite().await?;
    assert_eq!(
        call_id(&invite),
        pushed_call_id,
        "push carries the INVITE Call-ID"
    );
    assert!(invite.uri.to_string().contains("pn-prid=token-1"));

    let (alice, result) = call.await?;
    let dialog_id = result?;
    alice.hangup(&dialog_id).await.ok();
    Ok(())
}

/// The server cannot see a UDP phone die, so the INVITE goes to the dead
/// address — and the phone is pushed as well. It comes back on a new flow and
/// the INVITE follows it there.
#[tokio::test]
async fn invite_follows_device_to_its_new_flow() -> Result<()> {
    let mut server = PushTestServer::start(PushConfig {
        wakeup_timeout_secs: 10,
        ..Default::default()
    })
    .await?;

    let phone = Phone::launch_udp(server.udp_addr, "token-2").await?;
    phone.kill();

    let alice = server.caller().await?;
    let call = tokio::spawn(async move {
        let result = alice.make_call("bob", Some(caller_sdp())).await;
        (alice, result)
    });

    let (params, notification) = server.next_push().await?;
    assert_eq!(params.prid, "token-2");
    let mut relaunched = Phone::launch(server.tcp_addr, "token-2").await?;
    // The stale binding of the old flow is replaced, not kept beside it.
    server.wait_bob_bindings(1).await?;
    let invite = relaunched.answer_next_invite().await?;
    assert_eq!(Some(call_id(&invite)), notification.call_id);

    let (alice, result) = call.await?;
    let dialog_id = result?;
    alice.hangup(&dialog_id).await.ok();
    Ok(())
}

/// Like above, but the phone takes a while to relaunch: the INVITE to the
/// dead address times out first (Timer B, shortened here), and the call waits
/// for the device to re-register instead of failing.
#[tokio::test]
async fn invite_waits_for_device_when_its_flow_is_dead() -> Result<()> {
    let mut server = PushTestServer::start_with(
        PushConfig {
            wakeup_timeout_secs: 10,
            ..Default::default()
        },
        |config| config.t1x64_timer = Some(800),
    )
    .await?;

    let phone = Phone::launch_udp(server.udp_addr, "token-5").await?;
    phone.kill();

    let alice = server.caller().await?;
    let call = tokio::spawn(async move {
        let result = alice.make_call("bob", Some(caller_sdp())).await;
        (alice, result)
    });

    server.next_push().await?;
    sleep(Duration::from_millis(1500)).await;
    let mut relaunched = Phone::launch(server.tcp_addr, "token-5").await?;
    relaunched.answer_next_invite().await?;

    let (alice, result) = call.await?;
    let dialog_id = result?;
    alice.hangup(&dialog_id).await.ok();
    Ok(())
}

/// A pushed device that never comes back fails the call with 480 once the
/// wake-up timeout expires.
#[tokio::test]
async fn unanswered_push_fails_with_480() -> Result<()> {
    let mut server = PushTestServer::start(PushConfig {
        wakeup_timeout_secs: 2,
        ..Default::default()
    })
    .await?;

    let phone = Phone::launch(server.tcp_addr, "token-3").await?;
    phone.kill();
    server.wait_bob_asleep(1).await?;

    let alice = server.caller().await?;
    let started = std::time::Instant::now();
    let call = tokio::spawn(async move { alice.make_call("bob", Some(caller_sdp())).await });
    server.next_push().await?;

    let error = call.await?.expect_err("call must fail");
    assert!(error.to_string().contains("480"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(8));
    Ok(())
}

/// The caller hangs up while the device is being woken: the held INVITE is
/// dropped and never reaches the device once it registers.
#[tokio::test]
async fn caller_cancel_releases_held_invite() -> Result<()> {
    let mut server = PushTestServer::start(PushConfig {
        wakeup_timeout_secs: 10,
        ..Default::default()
    })
    .await?;

    let phone = Phone::launch(server.tcp_addr, "token-6").await?;
    phone.kill();
    server.wait_bob_asleep(1).await?;

    let alice = server.caller().await?;
    let call = tokio::spawn(async move { alice.make_call("bob", Some(caller_sdp())).await });
    server.next_push().await?;
    // Dropping the pending INVITE makes the caller send CANCEL.
    call.abort();
    sleep(Duration::from_millis(500)).await;

    let mut relaunched = Phone::launch(server.tcp_addr, "token-6").await?;
    assert!(
        timeout(Duration::from_secs(2), relaunched.invites.recv())
            .await
            .is_err(),
        "a cancelled call must not reach the device"
    );
    Ok(())
}

/// Two sleeping devices of the same user are both pushed; the one that wakes
/// up gets the call while the other one's fork is still waiting.
#[tokio::test]
async fn only_the_device_that_wakes_up_rings() -> Result<()> {
    let mut server = PushTestServer::start(PushConfig {
        wakeup_timeout_secs: 10,
        ..Default::default()
    })
    .await?;

    let phone_a = Phone::launch_instance(
        server.tcp_addr,
        "a0000000-0000-4000-8000-00000000000a",
        "token-a",
    )
    .await?;
    let phone_b = Phone::launch_instance(
        server.tcp_addr,
        "b0000000-0000-4000-8000-00000000000b",
        "token-b",
    )
    .await?;
    phone_a.kill();
    phone_b.kill();
    server.wait_bob_asleep(2).await?;

    let alice = server.caller().await?;
    let call = tokio::spawn(async move {
        let result = alice.make_call("bob", Some(caller_sdp())).await;
        (alice, result)
    });

    let mut pushed = vec![
        server.next_push().await?.0.prid,
        server.next_push().await?.0.prid,
    ];
    pushed.sort();
    assert_eq!(pushed, vec!["token-a".to_string(), "token-b".to_string()]);

    let mut relaunched = Phone::launch_instance(
        server.tcp_addr,
        "b0000000-0000-4000-8000-00000000000b",
        "token-b",
    )
    .await?;
    relaunched.answer_next_invite().await?;

    let (alice, result) = call.await?;
    let dialog_id = result?;
    alice.hangup(&dialog_id).await.ok();
    Ok(())
}

/// A route that redirects the dialed number to bob's extension wakes bob's
/// sleeping phone like a direct call does.
#[tokio::test]
async fn route_redirect_wakes_sleeping_device() -> Result<()> {
    use crate::proxy::routing::{MatchConditions, RewriteRules, RouteAction, RouteRule};
    let mut server = PushTestServer::start_with(
        PushConfig {
            wakeup_timeout_secs: 10,
            ..Default::default()
        },
        |config| {
            config.routes = Some(vec![RouteRule {
                name: "front-door".to_string(),
                priority: 10,
                match_conditions: MatchConditions {
                    to_user: Some("front-door".to_string()),
                    ..Default::default()
                },
                rewrite: Some(RewriteRules {
                    to_user: Some("bob".to_string()),
                    ..Default::default()
                }),
                action: RouteAction::default(),
                ..Default::default()
            }]);
        },
    )
    .await?;

    let phone = Phone::launch(server.tcp_addr, "token-7").await?;
    phone.kill();
    server.wait_bob_asleep(1).await?;

    let alice = server.caller().await?;
    let call = tokio::spawn(async move {
        let result = alice.make_call("front-door", Some(caller_sdp())).await;
        (alice, result)
    });

    let (params, notification) = server.next_push().await?;
    assert_eq!(params.prid, "token-7");
    let mut relaunched = Phone::launch(server.tcp_addr, "token-7").await?;
    let invite = relaunched.answer_next_invite().await?;
    assert_eq!(Some(call_id(&invite)), notification.call_id);

    let (alice, result) = call.await?;
    let dialog_id = result?;
    alice.hangup(&dialog_id).await.ok();
    Ok(())
}

/// A queue whose agent is bob wakes bob's sleeping phone for the agent leg.
#[tokio::test]
async fn queue_wakes_sleeping_agent() -> Result<()> {
    use crate::proxy::routing::{
        MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
        RouteQueueTargetConfig, RouteRule,
    };
    let mut server = PushTestServer::start_with(
        PushConfig {
            wakeup_timeout_secs: 10,
            ..Default::default()
        },
        |config| {
            config.queues.insert(
                "support".to_string(),
                RouteQueueConfig {
                    name: Some("support".to_string()),
                    strategy: RouteQueueStrategyConfig {
                        targets: vec![RouteQueueTargetConfig {
                            uri: "sip:bob@127.0.0.1".to_string(),
                            label: None,
                        }],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
            config.routes = Some(vec![RouteRule {
                name: "support".to_string(),
                priority: 10,
                match_conditions: MatchConditions {
                    to_user: Some("support".to_string()),
                    ..Default::default()
                },
                action: RouteAction {
                    queue: Some("support".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            }]);
        },
    )
    .await?;

    let phone = Phone::launch(server.tcp_addr, "token-8").await?;
    phone.kill();
    server.wait_bob_asleep(1).await?;

    let alice = server.caller().await?;
    let call = tokio::spawn(async move {
        let result = alice.make_call("support", Some(caller_sdp())).await;
        (alice, result)
    });

    let (params, notification) = server.next_push().await?;
    assert_eq!(params.prid, "token-8");
    let mut relaunched = Phone::launch(server.tcp_addr, "token-8").await?;
    let invite = relaunched.answer_next_invite().await?;
    assert_eq!(Some(call_id(&invite)), notification.call_id);

    let (alice, result) = call.await?;
    let dialog_id = result?;
    alice.hangup(&dialog_id).await.ok();
    Ok(())
}

/// RFC 8599 REGISTER handling: capability queries, unsupported providers
/// (555 when configured), and unregistration forgetting the device.
#[tokio::test]
async fn register_procedures() -> Result<()> {
    let server = PushTestServer::start(PushConfig {
        reject_unsupported_provider: true,
        ..Default::default()
    })
    .await?;
    let push = server.server.push.clone().expect("push service");
    let phone = Phone::start().await?;

    // Query for all supported push services.
    let response = phone.register(server.tcp_addr, ";pn-provider", 60).await?;
    assert_eq!(response.status_code, StatusCode::OK);
    assert_eq!(
        header_values(&response, "Feature-Caps"),
        vec!["*;+sip.pns=\"fcm\"".to_string()]
    );
    assert!(
        push.registry().is_empty(),
        "a query does not register a push device"
    );

    // A push service this PBX does not support.
    let response = phone
        .register(
            server.tcp_addr,
            ";pn-provider=apns;pn-prid=abc;pn-param=TEAM.app.voip",
            60,
        )
        .await?;
    assert_eq!(response.status_code.code(), 555);

    // Push binding capped by `max_expires`, not `max_registrar_expires`.
    let response = phone
        .register(
            server.tcp_addr,
            ";pn-provider=fcm;pn-prid=token-4",
            31_536_000,
        )
        .await?;
    assert_eq!(response.status_code, StatusCode::OK);
    let expires = response
        .expires_header()
        .map(|h| h.value().to_string())
        .unwrap_or_default();
    assert_eq!(expires, PushConfig::default().max_expires.to_string());
    assert_eq!(push.registry().len(), 1);

    // Unregistering the binding forgets the device.
    let response = phone
        .register(server.tcp_addr, ";pn-provider=fcm;pn-prid=token-4", 0)
        .await?;
    assert_eq!(response.status_code, StatusCode::OK);
    assert!(push.registry().is_empty());
    Ok(())
}
