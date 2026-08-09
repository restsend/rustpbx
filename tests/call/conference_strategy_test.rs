//! Conference Strategy lifecycle integration tests
//!
//! Verifies the full `MediaPathStrategy` decision flow as the active leg set
//! of a session changes: P2P → MCU → P2P → none, using a mock session that
//! implements [`LegMediaBridger`] against a real [`ConferenceServer`].

use std::sync::Arc;

use rustpbx::call::domain::LegId;
use rustpbx::call::runtime::{
    ConferenceId, ConferenceManager, ConferenceServer, ConferenceStrategy, LegMediaBridger,
    MediaPathContext, MediaPathDecision, MediaPathStrategy, SessionId,
};

/// Mock session that tracks which legs it bridged into a conference.
struct MockSession {
    server: Arc<ConferenceServer>,
    bridged: std::sync::Arc<std::sync::Mutex<Vec<LegId>>>,
    conf_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl LegMediaBridger for MockSession {
    async fn bridge_into(&mut self, conf_id: &str, leg_id: &LegId) -> anyhow::Result<()> {
        self.server
            .add_participant(&ConferenceId::from(conf_id), leg_id.clone())
            .await?;
        self.bridged.lock().unwrap().push(leg_id.clone());
        *self.conf_id.lock().unwrap() = Some(conf_id.to_string());
        Ok(())
    }

    async fn unbridge(&mut self, conf_id: &str, leg_id: &LegId) -> anyhow::Result<()> {
        let _ = self.server.leave_conference(conf_id, leg_id).await;
        self.bridged.lock().unwrap().retain(|l| l != leg_id);
        Ok(())
    }
}

fn ctx(session_id: &str, legs: Vec<LegId>) -> MediaPathContext {
    MediaPathContext {
        session_id: SessionId::from(session_id),
        active_legs: legs,
    }
}

#[tokio::test]
async fn test_p2p_to_mcu_to_p2p_lifecycle() {
    let manager = Arc::new(ConferenceManager::new());
    let server = Arc::new(ConferenceServer::new(manager.clone()));
    let strategy = ConferenceStrategy::new(server.clone());
    let mut session = MockSession {
        server: server.clone(),
        bridged: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        conf_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
    };

    // Phase 1: 2 legs → Direct
    let c2 = ctx("session-x", vec![LegId::new("a"), LegId::new("b")]);
    assert_eq!(
        strategy.decide(&c2.active_legs).unwrap(),
        MediaPathDecision::Direct(vec![LegId::new("a"), LegId::new("b")])
    );

    // Phase 2: 3 legs → Conference; strategy bridges all three
    let c3 = ctx(
        "session-x",
        vec![LegId::new("a"), LegId::new("b"), LegId::new("c")],
    );
    assert_eq!(strategy.decide(&c3.active_legs).unwrap(), MediaPathDecision::Conference);
    strategy.apply_multi_party(&c3, &mut session).await.unwrap();

    let conf_id = strategy.conference_id_for(&SessionId::from("session-x"));
    let conf = manager
        .get_conference(&ConferenceId::from(conf_id.as_str()))
        .await
        .unwrap();
    assert_eq!(conf.participant_count(), 3);
    assert_eq!(session.bridged.lock().unwrap().len(), 3);

    // Phase 3: back to 2 legs → Direct; conference torn down
    let c2b = ctx("session-x", vec![LegId::new("a"), LegId::new("b")]);
    assert_eq!(
        strategy.decide(&c2b.active_legs).unwrap(),
        MediaPathDecision::Direct(vec![LegId::new("a"), LegId::new("b")])
    );
    strategy.leave_multi_party(&c2b, &mut session).await.unwrap();

    assert!(
        manager
            .get_conference(&ConferenceId::from(conf_id.as_str()))
            .await
            .is_none(),
        "Auto conference should be destroyed after leaving multi-party"
    );

    // Phase 4: 1 leg → None
    let c1 = ctx("session-x", vec![LegId::new("a")]);
    assert_eq!(strategy.decide(&c1.active_legs).unwrap(), MediaPathDecision::None);
}

#[tokio::test]
async fn test_growing_conference_keeps_existing_participants() {
    let manager = Arc::new(ConferenceManager::new());
    let server = Arc::new(ConferenceServer::new(manager.clone()));
    let strategy = ConferenceStrategy::new(server.clone());
    let mut session = MockSession {
        server: server.clone(),
        bridged: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        conf_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
    };

    // 3 legs → conference
    let c3 = ctx(
        "session-y",
        vec![LegId::new("a"), LegId::new("b"), LegId::new("c")],
    );
    strategy.apply_multi_party(&c3, &mut session).await.unwrap();

    // 4 legs → apply again; only the new leg gets bridged (idempotent)
    let c4 = ctx(
        "session-y",
        vec![LegId::new("a"), LegId::new("b"), LegId::new("c"), LegId::new("d")],
    );
    assert_eq!(strategy.decide(&c4.active_legs).unwrap(), MediaPathDecision::Conference);
    strategy.apply_multi_party(&c4, &mut session).await.unwrap();

    let conf_id = strategy.conference_id_for(&SessionId::from("session-y"));
    let conf = manager
        .get_conference(&ConferenceId::from(conf_id.as_str()))
        .await
        .unwrap();
    assert_eq!(conf.participant_count(), 4);
    assert_eq!(session.bridged.lock().unwrap().len(), 4);
}
