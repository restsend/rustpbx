//! Queue Media E2E Test
//!
//! Verifies that a call routed into a **queue** and answered by an agent has
//! working **bidirectional** RTP through the proxy in anchored
//! (`media_proxy = all`) mode.
//!
//! This exercises the app-runtime queue path (inbound route → `queue` action →
//! `app_runtime.start_app("queue")`), which dials agents as *dynamically-added
//! legs* (`LegAdd` / `initiate_sip_leg`) rather than the fixed callee leg. That
//! path is where a whole class of media bugs lived undetected, because the
//! existing queue tests only asserted **signaling** (an agent received the
//! INVITE) and never that **audio actually flows both ways** once the agent
//! answers. Concretely, this test guards against:
//!
//! - early-media (183) consuming the leg's `HaveLocalOffer` so the final 200 OK
//!   answer fails to apply, leaving the agent leg on the wrong codec/endpoint
//!   and producing **no agent→caller audio**;
//! - dynamic legs never being wired into media forwarding at all.
//!
//! The single assertion that matters — and that no prior queue test made — is
//! that **both** the caller and the agent receive RTP packets.

use super::e2e_test_server::E2eTestServer;
use super::rtp_utils::{RtpReceiver, RtpSender, RtpStats, extract_media_endpoint};
use super::test_helpers::pcmu_sdp;
use super::test_ua::{TestUa, TestUaEvent};
use crate::config::{MediaProxyMode, ProxyConfig};
use crate::proxy::routing::{
    MatchConditions, RouteAction, RouteQueueConfig, RouteQueueStrategyConfig,
    RouteQueueTargetConfig, RouteRule,
};
use anyhow::{Result, anyhow};
use rsipstack::dialog::DialogId;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

/// Build a ProxyConfig with a single queue "support" whose only agent is the
/// registered user `bob`, plus a route that sends calls to `support` into it.
fn queue_proxy_config() -> ProxyConfig {
    let mut config = ProxyConfig {
        media_proxy: MediaProxyMode::All,
        // Match the E2E media test harness: disable RTP latching so the proxy
        // sends to the SDP-advertised receiver ports (the test's RtpReceivers)
        // rather than latching onto the RtpSenders' source ports.
        enable_latching: false,
        ..Default::default()
    };

    // Queue "support" → dial agent bob (resolved via the locator at call time).
    let queue_config = RouteQueueConfig {
        name: Some("support".to_string()),
        strategy: RouteQueueStrategyConfig {
            targets: vec![RouteQueueTargetConfig {
                uri: "sip:bob@127.0.0.1".to_string(),
                label: Some("Support Agent".to_string()),
            }],
            wait_timeout_secs: Some(10),
            ..Default::default()
        },
        // Do NOT answer the caller up front — this is the production-relevant
        // path (caller answered only when the agent connects), and the one that
        // exercised the early-media / HaveLocalOffer bug.
        accept_immediately: false,
        ..Default::default()
    };
    config.queues.insert("support".to_string(), queue_config);

    // Route: dial "support" → queue "support".
    let queue_route = RouteRule {
        name: "route_to_support".to_string(),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("support".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            queue: Some("support".to_string()),
            auto_answer: true,
            ..Default::default()
        },
        ..Default::default()
    };
    config.routes = Some(vec![queue_route]);
    config
}

struct QueueMediaTestCtx {
    server: Arc<E2eTestServer>,
    caller_ua: TestUa,
    agent_ua: TestUa,
    caller_sender: RtpSender,
    caller_receiver: RtpReceiver,
    agent_sender: RtpSender,
    agent_receiver: RtpReceiver,
}

impl QueueMediaTestCtx {
    async fn setup() -> Result<Self> {
        let server = Arc::new(E2eTestServer::start_with_config(queue_proxy_config()).await?);

        // alice = caller, bob = the queue's agent. Both register so the queue
        // can resolve bob's contact via the locator.
        let caller_ua = server.create_ua("alice").await?;
        let agent_ua = server.create_ua("bob").await?;

        sleep(Duration::from_millis(100)).await;

        let caller_sender = RtpSender::bind().await?;
        let caller_receiver = RtpReceiver::bind(0).await?;
        let agent_sender = RtpSender::bind().await?;
        let agent_receiver = RtpReceiver::bind(0).await?;

        Ok(Self {
            server,
            caller_ua,
            agent_ua,
            caller_sender,
            caller_receiver,
            agent_sender,
            agent_receiver,
        })
    }

    /// Caller dials the queue ("support"); the queue hunts the agent (bob), who
    /// answers. Returns (caller_dialog_id, agent_dialog_id, agent_offer_sdp).
    async fn establish_queue_call(
        &self,
        caller_sdp: String,
        agent_sdp: String,
    ) -> Result<(DialogId, DialogId, String)> {
        let caller = Arc::new(self.caller_ua.clone());
        let caller_handle =
            crate::utils::spawn(async move { caller.make_call("support", Some(caller_sdp)).await });

        // The agent leg is dialed by the queue; wait for bob's INVITE and answer.
        let mut agent: Option<(DialogId, String)> = None;
        for _ in 0..100 {
            let events = self.agent_ua.process_dialog_events().await?;
            for event in events {
                if let TestUaEvent::IncomingCall(id, offer) = event {
                    self.agent_ua.answer_call(&id, Some(agent_sdp.clone())).await?;
                    info!("Agent answered queue call");
                    agent = Some((id, offer.unwrap_or_default()));
                    break;
                }
            }
            if agent.is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        let (agent_id, agent_offer) =
            agent.ok_or_else(|| anyhow!("Agent never received INVITE from queue"))?;

        let caller_id = tokio::time::timeout(Duration::from_secs(8), caller_handle)
            .await
            .map_err(|_| anyhow!("Caller timed out waiting for queue connect"))?
            .map_err(|e| anyhow!("Caller task join error: {}", e))?
            .map_err(|e| anyhow!("Queue call failed: {}", e))?;

        Ok((caller_id, agent_id, agent_offer))
    }

    /// Poll the caller's dialog events until its call is terminated, or fail.
    async fn wait_for_caller_terminated(&self, caller_id: &DialogId, secs: u64) -> Result<()> {
        for _ in 0..(secs * 10) {
            let events = self.caller_ua.process_dialog_events().await?;
            for event in events {
                if let TestUaEvent::CallTerminated(id) = event {
                    if &id == caller_id {
                        return Ok(());
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(anyhow!(
            "Caller call did not terminate after agent hung up (hangup cascade broken)"
        ))
    }

    async fn exchange_rtp(
        &self,
        caller_target: SocketAddr,
        agent_target: SocketAddr,
        payload_type: u8,
        duration_ms: u64,
    ) -> Result<(RtpStats, RtpStats)> {
        use super::rtp_utils::RtpPacket;

        let packet_interval_ms: u64 = 20;
        let packet_count = (duration_ms / packet_interval_ms) as usize;

        let caller_packets = RtpPacket::create_sequence(
            packet_count, 1000, 50000, 0xA1A1A1A1, payload_type, 160, 160,
        );
        let agent_packets = RtpPacket::create_sequence(
            packet_count, 2000, 60000, 0xB2B2B2B2, payload_type, 160, 160,
        );

        self.caller_receiver.start_receiving();
        self.agent_receiver.start_receiving();

        self.caller_sender
            .start_sending(agent_target, caller_packets, packet_interval_ms);
        self.agent_sender
            .start_sending(caller_target, agent_packets, packet_interval_ms);

        sleep(Duration::from_millis(duration_ms + 500)).await;

        self.caller_sender.stop();
        self.agent_sender.stop();
        sleep(Duration::from_millis(200)).await;

        Ok((
            self.caller_receiver.get_stats().await,
            self.agent_receiver.get_stats().await,
        ))
    }
}

/// A call routed through a queue and answered by an agent must have working
/// bidirectional RTP through the proxy in anchored mode.
#[tokio::test]
async fn test_queue_call_bidirectional_rtp() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let ctx = QueueMediaTestCtx::setup().await?;

    let caller_port = ctx.caller_receiver.port().unwrap();
    let agent_port = ctx.agent_receiver.port().unwrap();

    let caller_sdp = pcmu_sdp("127.0.0.1", caller_port);
    let agent_sdp = pcmu_sdp("127.0.0.1", agent_port);

    // 1. Caller dials the queue; agent answers.
    let (caller_id, _agent_id, agent_offer) =
        ctx.establish_queue_call(caller_sdp, agent_sdp).await?;
    info!(%caller_id, "Queue call connected to agent");

    // 2. Resolve the proxy-anchored RTP endpoints for both legs.
    let caller_answer_sdp = ctx
        .caller_ua
        .get_negotiated_answer_sdp(&caller_id)
        .await
        .ok_or_else(|| anyhow!("No answer SDP on caller side"))?;

    let agent_target = extract_media_endpoint(&agent_offer)
        .ok_or_else(|| anyhow!("Failed to parse agent-side proxy media endpoint"))?;
    let caller_target = extract_media_endpoint(&caller_answer_sdp)
        .ok_or_else(|| anyhow!("Failed to parse caller-side proxy media endpoint"))?;

    info!(%caller_target, %agent_target, "Anchored RTP endpoints resolved");

    // 3. Exchange RTP for ~2s (PCMU).
    let (caller_stats, agent_stats) = ctx
        .exchange_rtp(caller_target, agent_target, 0, 2000)
        .await?;

    info!(
        caller_received = caller_stats.packets_received,
        agent_received = agent_stats.packets_received,
        "Queue RTP exchange complete"
    );

    // 4. The assertions that matter: audio flows BOTH ways.
    assert!(
        agent_stats.packets_received > 0,
        "Agent should receive RTP from caller through the queue bridge (got 0)"
    );
    assert!(
        caller_stats.packets_received > 0,
        "Caller should receive RTP from the agent through the queue bridge (got 0) \
         — this is the regression that produced one-way (caller-only) audio"
    );

    assert!(
        agent_stats.payload_types.contains(&0),
        "Agent should receive PCMU (PT 0), got {:?}",
        agent_stats.payload_types
    );
    assert!(
        caller_stats.payload_types.contains(&0),
        "Caller should receive PCMU (PT 0) from agent, got {:?}",
        caller_stats.payload_types
    );

    ctx.caller_ua.hangup(&caller_id).await?;
    ctx.server.stop();
    Ok(())
}

/// When the agent hangs up, the caller's call must be torn down too (the
/// hangup must cascade through the queue bridge). The dynamic-leg path failed
/// to propagate the agent BYE, leaving the caller stuck in a live call.
#[tokio::test]
async fn test_queue_agent_hangup_ends_caller_call() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let ctx = QueueMediaTestCtx::setup().await?;

    let caller_sdp = pcmu_sdp("127.0.0.1", ctx.caller_receiver.port().unwrap());
    let agent_sdp = pcmu_sdp("127.0.0.1", ctx.agent_receiver.port().unwrap());

    let (caller_id, agent_id, _agent_offer) =
        ctx.establish_queue_call(caller_sdp, agent_sdp).await?;
    info!(%caller_id, %agent_id, "Queue call connected; agent will hang up");

    // Agent hangs up.
    ctx.agent_ua.hangup(&agent_id).await?;

    // The caller's call must terminate as a result.
    ctx.wait_for_caller_terminated(&caller_id, 5).await?;
    info!("Caller call terminated after agent hangup (cascade OK)");

    ctx.server.stop();
    Ok(())
}
