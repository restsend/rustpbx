//! Conference Media Path Strategy
//!
//! A [`MediaPathStrategy`] that routes two-party calls through a direct
//! bridge and three-or-more party calls through a conference / MCU managed
//! by [`ConferenceServer`].
//!
//! The strategy owns the MCU routing decision and delegates the mechanical
//! per-leg audio bridging to a [`LegMediaBridger`] provided by the session.

use std::sync::Arc;

use crate::call::domain::LegId;
use crate::call::runtime::{
    ConferenceId, ConferenceServer, LegMediaBridger, MediaPathContext, MediaPathDecision,
    MediaPathStrategy, SessionId,
};

/// Strategy that switches between P2P direct bridging and conference/MCU
/// routing based on the active leg count.
#[derive(Clone)]
pub struct ConferenceStrategy {
    server: Arc<ConferenceServer>,
    /// Owning session id — used by `shutdown()` to destroy the auto conference
    /// when the session is dropped without an explicit teardown.
    session_id: Option<String>,
}

impl ConferenceStrategy {
    pub fn new(server: Arc<ConferenceServer>) -> Self {
        Self {
            server,
            session_id: None,
        }
    }

    /// Bind this strategy to a session id so `shutdown()` can clean up the
    /// session's auto conference.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Access the underlying [`ConferenceServer`].
    pub fn server(&self) -> &Arc<ConferenceServer> {
        &self.server
    }

    /// Auto-conference id for a session (mirrors the old `conf-{session_id}`).
    pub fn conference_id_for(&self, session_id: &SessionId) -> String {
        format!("conf-{}", session_id.0)
    }
}

#[async_trait::async_trait]
impl MediaPathStrategy for ConferenceStrategy {
    fn decide(&self, active_legs: &[LegId]) -> anyhow::Result<MediaPathDecision> {
        Ok(match active_legs.len() {
            2 => MediaPathDecision::Direct(active_legs.to_vec()),
            0 | 1 => MediaPathDecision::None,
            _ => MediaPathDecision::Conference,
        })
    }

    async fn apply_multi_party(
        &self,
        ctx: &MediaPathContext,
        bridger: &mut (dyn LegMediaBridger + Send + Sync),
    ) -> anyhow::Result<()> {
        let conf_id = self.conference_id_for(&ctx.session_id);

        if self
            .server
            .get_conference(&ConferenceId::from(conf_id.as_str()))
            .await
            .is_none()
        {
            self.server
                .create_conference(ConferenceId::from(conf_id.as_str()), None)
                .await?;
        }

        for leg in &ctx.active_legs {
            // Idempotent: skip legs already bridged into this conference.
            if let Some(existing) = self.server.get_conference_id_for_leg(leg).await {
                if existing.0 == conf_id {
                    continue;
                }
                anyhow::bail!(
                    "Leg {} is already in conference {}, cannot bridge into {}",
                    leg,
                    existing.0,
                    conf_id
                );
            }
            bridger.bridge_into(&conf_id, leg).await?;
        }

        Ok(())
    }

    async fn leave_multi_party(
        &self,
        ctx: &MediaPathContext,
        bridger: &mut (dyn LegMediaBridger + Send + Sync),
    ) -> anyhow::Result<()> {
        let conf_id = self.conference_id_for(&ctx.session_id);

        for leg in &ctx.active_legs {
            bridger.unbridge(&conf_id, leg).await?;
        }

        // Destroy the auto conference when leaving multi-party routing.
        let _ = self
            .server
            .destroy_conference(&ConferenceId::from(conf_id.as_str()))
            .await;
        Ok(())
    }

    fn shutdown(&self) {
        // Best-effort teardown of the session's auto conference so the audio
        // mixer and participant registrations don't leak when the session is
        // dropped without a clean `leave_multi_party`. Runs on the current
        // tokio runtime if one is available; no-ops during runtime teardown.
        let Some(sid) = self.session_id.as_ref() else { return };
        let conf_id = self.conference_id_for(&SessionId::from(sid.clone()));
        let server = self.server.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = server
                    .destroy_conference(&ConferenceId::from(conf_id.as_str()))
                    .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::runtime::SessionId;
    use dashmap::DashMap;

    #[derive(Default)]
    struct RecordingBridger {
        server: Option<Arc<ConferenceServer>>,
        bridges: Option<Arc<DashMap<String, usize>>>,
    }

    impl RecordingBridger {
        fn new(server: Arc<ConferenceServer>) -> Self {
            Self {
                server: Some(server),
                bridges: Some(Arc::new(DashMap::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl LegMediaBridger for RecordingBridger {
        async fn bridge_into(&mut self, conf_id: &str, leg_id: &LegId) -> anyhow::Result<()> {
            let server = self.server.as_ref().unwrap();
            server
                .add_participant(&ConferenceId::from(conf_id), leg_id.clone())
                .await?;
            let map = self.bridges.as_ref().unwrap();
            *map.entry(conf_id.to_string()).or_insert(0) += 1;
            Ok(())
        }
        async fn unbridge(&mut self, conf_id: &str, leg_id: &LegId) -> anyhow::Result<()> {
            if let Some(server) = self.server.as_ref() {
                let _ = server.leave_conference(conf_id, leg_id).await;
            }
            Ok(())
        }
    }

    fn ctx(session_id: &str, legs: Vec<LegId>) -> MediaPathContext {
        MediaPathContext {
            session_id: SessionId::from(session_id),
            active_legs: legs,
        }
    }

    #[test]
    fn decide_routes_by_leg_count() {
        let server = Arc::new(ConferenceServer::new(Arc::new(
            crate::call::runtime::ConferenceManager::new(),
        )));
        let s = ConferenceStrategy::new(server);
        assert_eq!(s.decide(&[]).unwrap(), MediaPathDecision::None);
        assert_eq!(s.decide(&[LegId::new("a")]).unwrap(), MediaPathDecision::None);
        assert_eq!(
            s.decide(&[LegId::new("a"), LegId::new("b")]).unwrap(),
            MediaPathDecision::Direct(vec![LegId::new("a"), LegId::new("b")])
        );
        assert_eq!(
            s.decide(&[LegId::new("a"), LegId::new("b"), LegId::new("c")]).unwrap(),
            MediaPathDecision::Conference
        );
    }

    #[tokio::test]
    async fn apply_multi_party_creates_conference_and_bridges_legs() {
        let manager = Arc::new(crate::call::runtime::ConferenceManager::new());
        let server = Arc::new(ConferenceServer::new(manager.clone()));
        let s = ConferenceStrategy::new(server.clone());

        let c = ctx(
            "session-1",
            vec![LegId::new("a"), LegId::new("b"), LegId::new("c")],
        );
        let mut bridger = RecordingBridger::new(server.clone());

        s.apply_multi_party(&c, &mut bridger).await.unwrap();

        let conf_id = s.conference_id_for(&SessionId::from("session-1"));
        let conf = manager
            .get_conference(&ConferenceId::from(conf_id.as_str()))
            .await
            .unwrap();
        assert_eq!(conf.participant_count(), 3);
        assert_eq!(
            bridger.bridges.as_ref().unwrap().get(&conf_id).unwrap().value(),
            &3
        );
    }

    #[tokio::test]
    async fn apply_multi_party_is_idempotent() {
        let manager = Arc::new(crate::call::runtime::ConferenceManager::new());
        let server = Arc::new(ConferenceServer::new(manager.clone()));
        let s = ConferenceStrategy::new(server.clone());

        let c = ctx(
            "session-2",
            vec![LegId::new("a"), LegId::new("b"), LegId::new("c")],
        );
        let mut bridger = RecordingBridger::new(server.clone());

        s.apply_multi_party(&c, &mut bridger).await.unwrap();
        // Second apply with same legs must not re-bridge already-bridged legs.
        s.apply_multi_party(&c, &mut bridger).await.unwrap();

        let conf_id = s.conference_id_for(&SessionId::from("session-2"));
        assert_eq!(
            bridger.bridges.as_ref().unwrap().get(&conf_id).unwrap().value(),
            &3
        );
    }

    #[tokio::test]
    async fn leave_multi_party_destroys_conference() {
        let manager = Arc::new(crate::call::runtime::ConferenceManager::new());
        let server = Arc::new(ConferenceServer::new(manager.clone()));
        let s = ConferenceStrategy::new(server.clone());

        let c = ctx(
            "session-3",
            vec![LegId::new("a"), LegId::new("b"), LegId::new("c")],
        );
        let mut bridger = RecordingBridger::new(server.clone());

        s.apply_multi_party(&c, &mut bridger).await.unwrap();
        s.leave_multi_party(&c, &mut bridger).await.unwrap();

        let conf_id = s.conference_id_for(&SessionId::from("session-3"));
        assert!(
            manager
                .get_conference(&ConferenceId::from(conf_id.as_str()))
                .await
                .is_none()
        );
    }
}
