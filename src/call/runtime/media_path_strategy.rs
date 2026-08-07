//! Media Path Strategy
//!
//! Decides how a call session routes media as its active leg set changes.
//!
//! The goal is to keep multi-party (MCU / conference) knowledge out of
//! `SipSession`. The session holds an `Arc<dyn MediaPathStrategy>` and asks
//! it (a) which routing to use and (b) to apply / tear down multi-party
//! routing via a [`LegMediaBridger`] that the session implements from its own
//! media primitives (MediaBridge / peers / SDP).

use tokio_util::sync::CancellationToken;

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
    pub cancel_token: CancellationToken,
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
/// `bridge_into` (e.g. via `ConferenceServer::join_conference_with_media` or
/// `ConferenceManager::add_participant`). The default strategies rely on
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

/// Strategy with no multi-party support.
///
/// Two active legs → direct P2P bridge. Any other count → no routing. Calling
/// `apply_multi_party` fails because this strategy has no MCU backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct P2POnlyStrategy;

#[async_trait::async_trait]
impl MediaPathStrategy for P2POnlyStrategy {
    fn decide(&self, active_legs: &[LegId]) -> anyhow::Result<MediaPathDecision> {
        match active_legs.len() {
            2 => Ok(MediaPathDecision::Direct(active_legs.to_vec())),
            0 | 1 => Ok(MediaPathDecision::None),
            n => {
                tracing::warn!(
                    active_legs = n,
                    "P2POnlyStrategy cannot route 3+ legs; no multi-party support configured"
                );
                Ok(MediaPathDecision::None)
            }
        }
    }

    async fn apply_multi_party(
        &self,
        _ctx: &MediaPathContext,
        _bridger: &mut (dyn LegMediaBridger + Send + Sync),
    ) -> anyhow::Result<()> {
        anyhow::bail!("P2POnlyStrategy does not support multi-party routing")
    }

    async fn leave_multi_party(
        &self,
        _ctx: &MediaPathContext,
        _bridger: &mut (dyn LegMediaBridger + Send + Sync),
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct MockBridger {
        bridge_calls: Arc<AtomicUsize>,
        unbridge_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LegMediaBridger for MockBridger {
        async fn bridge_into(&mut self, _conf_id: &str, _leg_id: &LegId) -> anyhow::Result<()> {
            self.bridge_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn unbridge(&mut self, _conf_id: &str, _leg_id: &LegId) -> anyhow::Result<()> {
            self.unbridge_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn ctx(legs: Vec<LegId>) -> MediaPathContext {
        MediaPathContext {
            session_id: SessionId::from("test-session"),
            active_legs: legs,
            cancel_token: CancellationToken::new(),
        }
    }

    #[test]
    fn p2p_decide_two_legs_direct() {
        let s = P2POnlyStrategy;
        let legs = vec![LegId::new("a"), LegId::new("b")];
        assert_eq!(
            s.decide(&legs).unwrap(),
            MediaPathDecision::Direct(vec![LegId::new("a"), LegId::new("b")])
        );
    }

    #[test]
    fn p2p_decide_zero_one_legs_none() {
        let s = P2POnlyStrategy;
        assert_eq!(s.decide(&[]).unwrap(), MediaPathDecision::None);
        assert_eq!(s.decide(&[LegId::new("a")]).unwrap(), MediaPathDecision::None);
    }

    #[test]
    fn p2p_decide_three_plus_legs_none() {
        let s = P2POnlyStrategy;
        let legs = vec![LegId::new("a"), LegId::new("b"), LegId::new("c")];
        assert_eq!(s.decide(&legs).unwrap(), MediaPathDecision::None);
    }

    #[tokio::test]
    async fn p2p_apply_multi_party_unsupported() {
        let s = P2POnlyStrategy;
        let c = ctx(vec![LegId::new("a"), LegId::new("b"), LegId::new("c")]);
        let mut b = MockBridger::default();
        let result = s.apply_multi_party(&c, &mut b).await;
        assert!(result.is_err(), "P2P strategy should reject multi-party");
    }

    #[tokio::test]
    async fn p2p_leave_multi_party_noop() {
        let s = P2POnlyStrategy;
        let c = ctx(vec![LegId::new("a")]);
        let mut b = MockBridger::default();
        s.leave_multi_party(&c, &mut b).await.unwrap();
        assert_eq!(b.unbridge_calls.load(Ordering::SeqCst), 0);
    }
}
