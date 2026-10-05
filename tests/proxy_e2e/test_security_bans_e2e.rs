use std::time::Duration;

use rustpbx::config::{BanConfig, ProxyConfig};
use rustpbx::security::BanStore;

use crate::common::e2e_test_server::{E2eTestServer, E2eTestServerInject};
use crate::common::test_ua::{TestUa, TestUaConfig};

async fn migrated_db() -> sea_orm::DatabaseConnection {
    use sea_orm_migration::MigratorTrait;
    let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
    rustpbx::models::migration::Migrator::up(&db, None)
        .await
        .unwrap();
    db
}

#[tokio::test]
async fn bad_credentials_ban_the_source_and_release_restores_service() {
    let db = migrated_db().await;
    let store = BanStore::load(
        db,
        BanConfig {
            enabled: true,
            max_failures: 3,
            window_secs: 600,
            ban_durations_secs: vec![3600],
            protected_cidrs: Vec::new(),
        },
        None,
        None,
    )
    .await
    .unwrap();

    let server = E2eTestServer::start_with_inject(
        ProxyConfig::default(),
        E2eTestServerInject {
            bans: Some(store.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let cfg = TestUaConfig {
        webrtc: false,
        username: "alice".to_string(),
        password: "wrong-password".to_string(),
        realm: server.proxy_addr.ip().to_string(),
        local_port: portpicker::pick_unused_port().unwrap_or(27170),
        proxy_addr: server.proxy_addr,
    };
    let mut ua = TestUa::new(cfg);
    ua.start().await.unwrap();

    let source: std::net::IpAddr = server.proxy_addr.ip();
    for _ in 0..6 {
        let _ = ua.register().await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    for _ in 0..100 {
        if store.is_banned(&source) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        store.is_banned(&source),
        "repeated bad credentials must ban the source"
    );

    let active = store.list_active().await.unwrap();
    assert_eq!(active.len(), 1, "one ban row expected");
    assert_eq!(active[0].reason, "auth_failures");
    assert_eq!(active[0].offense_count, 1);

    let _ = ua.register().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        store.is_banned(&source),
        "banned source stays banned across further requests"
    );

    assert!(store.release(&source.to_string(), "test").await.unwrap());
    assert!(!store.is_banned(&source), "release clears the ban");
}
