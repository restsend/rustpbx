//! Media Path Strategy
//!
//! Decides how a call session routes media as its active leg set changes.
//!
//! The goal is to keep multi-party (MCU / conference) knowledge out of
//! `SipSession`. The session holds an `Arc<dyn MediaPathStrategy>` and asks
//! it (a) which routing to use and (b) to apply / tear down multi-party
//! routing via a [`LegMediaBridger`] that the session implements from its own
//! media primitives (MediaBridge / peers / SDP).

use crate::call::domain::LegId;
use crate::call::runtime::SessionId;

/// Routing decision for a session's media path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPathDecision {
    /// Bridge the given legs directly (2-party P2P). The session sets this up.
    Direct(Vec<LegId>),
    /// Route all active legs through a multi-party conference / MCU.
    /// The strategy applies this via `apply_multi_party`.
    Conference,
    /// No media routing.
    None,
}

/// Minimal view of a session's media state provided to a strategy.
#[derive(Clone, Debug)]
pub struct MediaPathContext {
    pub session_id: SessionId,
    pub active_legs: Vec<LegId>,
}

/// Bridges a single leg's media into a multi-party mixer.
///
/// Implemented by the session: given a conference id and a leg, it creates
/// the full-duplex audio bridge (forward: mixed audio → leg; reverse: leg
/// audio → mixer) using its own MediaBridge / peers.
///
/// # Implementation contract
///
/// Implementors MUST register the participant in the mixer as part of
/// `bridge_into` (e.g. via `ConferenceManager::add_participant`). The default strategies rely on
/// `get_conference_id_for_leg` returning the conference for idempotency
/// checks — if the participant is not registered, the strategy will re-bridge
/// the same leg on every routing change.
#[async_trait::async_trait]
pub trait LegMediaBridger: Send + Sync {
    async fn bridge_into(&mut self, conf_id: &str, leg_id: &LegId) -> anyhow::Result<()>;
    async fn unbridge(&mut self, conf_id: &str, leg_id: &LegId) -> anyhow::Result<()>;
}

/// Strategy that decides and manages media routing for a session.
#[async_trait::async_trait]
pub trait MediaPathStrategy: Send + Sync {
    /// Decide the desired routing for the given active legs.
    /// An `Err` means the strategy cannot route this leg set; callers should
    /// fall back to no routing (stop all bridges) and surface a warning.
    fn decide(&self, active_legs: &[LegId]) -> anyhow::Result<MediaPathDecision>;

    /// Apply multi-party routing: bridge all active legs into a conference.
    /// The strategy is responsible for creating/ensuring the conference and
    /// bridging each active leg via `bridger`.
    async fn apply_multi_party(
        &self,
        ctx: &MediaPathContext,
        bridger: &mut (dyn LegMediaBridger + Send + Sync),
    ) -> anyhow::Result<()>;

    /// Tear down any active multi-party routing.
    async fn leave_multi_party(
        &self,
        ctx: &MediaPathContext,
        bridger: &mut (dyn LegMediaBridger + Send + Sync),
    ) -> anyhow::Result<()>;

    /// Release resources owned by the strategy (session shutdown).
    fn shutdown(&self);
}


