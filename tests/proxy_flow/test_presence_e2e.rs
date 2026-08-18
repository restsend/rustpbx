use anyhow::Result;
use rustpbx::config::MediaProxyMode;
use rustpbx::proxy::cluster_event::EventSource;
use rustpbx::proxy::presence::{PresenceState, PresenceStatus};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use crate::common::e2e_test_server::E2eTestServer;

#[tokio::test]
async fn test_presence_manager_initial_offline() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_presence(MediaProxyMode::Auto).await?);

    let state = server.server_ref.presence_manager.get_state("bob");
    assert_eq!(
        state.status,
        PresenceStatus::Offline,
        "No PUBLISH yet → offline"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_presence_state_transition_offline_to_busy() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_presence(MediaProxyMode::Auto).await?);

    server
        .server_ref
        .presence_manager
        .update_state(
            "bob",
            PresenceState {
                status: PresenceStatus::Busy,
                note: Some("in a call".to_string()),
                activity: Some("busy".to_string()),
                last_updated: chrono::Utc::now().timestamp(),
            },
            &EventSource::Local,
        )
        .await;
    sleep(Duration::from_millis(100)).await;

    let state = server.server_ref.presence_manager.get_state("bob");
    assert_eq!(
        state.status,
        PresenceStatus::Busy,
        "should be busy after update"
    );
    assert_eq!(state.note.as_deref(), Some("in a call"));

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_presence_away_with_note() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let server = Arc::new(E2eTestServer::start_with_presence(MediaProxyMode::Auto).await?);

    server
        .server_ref
        .presence_manager
        .update_state(
            "charlie",
            PresenceState {
                status: PresenceStatus::Away("meeting".to_string()),
                note: Some("away:meeting".to_string()),
                activity: Some("away".to_string()),
                last_updated: chrono::Utc::now().timestamp(),
            },
            &EventSource::Local,
        )
        .await;
    sleep(Duration::from_millis(100)).await;

    let state = server.server_ref.presence_manager.get_state("charlie");
    assert!(
        matches!(state.status, PresenceStatus::Away(ref d) if d == "meeting"),
        "should be Away(meeting)"
    );

    server.stop();
    Ok(())
}
