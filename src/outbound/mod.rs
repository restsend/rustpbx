//! Predictive outbound calling — HTTP/SSE interface.
//!
//! Provides `POST /ami/v1/outbound/dial` which accepts a dial request,
//! originates a SIP call, and streams RWI call events via SSE until the call
//! is answered or fails. The SSE stream is a pure RWI event passthrough —
//! zero custom event types. On answer, a configurable post-answer action runs:
//!
//! - `execute_flow` — keep the call connected (default)
//! - `bridge_to_leg` — bridge the answered leg to an existing leg
//! - `enqueue` — place the answered leg into an ACD queue
//! - `webhook` — POST to a sync webhook that returns the next action
//!
//! See `request.rs` for the request schema.

pub mod api;
pub mod dispatcher;
pub mod error_catalog;
pub mod events;
pub mod request;
pub mod webhook;

pub use request::{DialRequest, OnAnswer, OnFailure};

use crate::call::runtime::ConferenceManager;
use crate::config::OutboundConfig;
use crate::proxy::active_call_registry::ActiveProxyCallRegistry;
use crate::proxy::server::SipServerRef;
use crate::rwi::RwiGatewayRef;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Context holding all dependencies needed to execute an outbound dial.
/// Constructable from `AppState` (production) or directly (tests).
#[derive(Clone)]
pub struct OutboundContext {
    pub sip_server: SipServerRef,
    pub gateway: RwiGatewayRef,
    pub call_registry: Arc<ActiveProxyCallRegistry>,
    pub conference_manager: Arc<ConferenceManager>,
    pub http_client: reqwest::Client,
    pub config: OutboundConfig,
    /// Enforces `[outbound] max_concurrent` — each dial holds a permit from
    /// originate until its SSE pump terminates. Shared from `AppState` so all
    /// requests contend on the same limiter.
    pub concurrency_limiter: Arc<Semaphore>,
}

impl OutboundContext {
    /// Build from an `AppState`. Returns `None` if required RWI components
    /// are not configured.
    pub fn from_app_state(app: &crate::app::AppState) -> Option<Self> {
        let config = app.config().outbound.clone()?;
        if !config.enabled {
            return None;
        }
        let gateway = app.core.rwi_gateway.clone()?;
        let call_registry = app.core.rwi_call_registry.clone()?;
        let sip_server = app.sip_server().get_inner();
        let conference_manager = sip_server.conference_manager.clone();
        // `outbound_limiter` is created at startup whenever `[outbound]` is
        // enabled; `None` here only happens in tests / exotic reload windows.
        // Fall back to an explicitly unlimited semaphore (NOT a fresh small
        // one — a per-request semaphore would silently defeat the limit).
        let concurrency_limiter = app.outbound_limiter.clone().unwrap_or_else(|| {
            tracing::debug!("outbound: no shared concurrency limiter on AppState — unlimited");
            Arc::new(Semaphore::new(usize::MAX))
        });
        Some(Self {
            sip_server,
            gateway,
            call_registry,
            conference_manager,
            http_client: app.http_client().clone(),
            config,
            concurrency_limiter,
        })
    }
}
