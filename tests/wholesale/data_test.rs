use axum::{Extension, Json};
use chrono::Utc;
use rustpbx::addons::wholesale::{
    cluster::api_add_peer,
    data::{CarrierCandidate, RateConfig, RateMatcher, WholesaleState},
    migration::Migrator as WholesaleMigrator,
    models::{
        rate, rate_deck, routing_profile, routing_profile_item, tenant, tenant_trunk,
        wholesale_trunk_config,
    },
};
use rustpbx::models::migration::Migrator as MainMigrator;
use rustpbx::models::sip_trunk;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;

async fn setup_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");

    MainMigrator::up(&db, None).await.expect("main migrations");
    WholesaleMigrator::up(&db, None)
        .await
        .expect("wholesale migrations");

    db
}

#[tokio::test]
async fn test_inbound_lookup_uses_trunk_id() {
    let db = setup_db().await;
    let state = WholesaleState::new();
    let broad = sip_trunk::ActiveModel {
        name: Set("Broad".to_string()),
        allowed_ips: Set(Some(serde_json::json!(["10.0.0.1"]))),
        incoming_to_user_prefix: Set(Some("86".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create broad inbound trunk");
    let specific = sip_trunk::ActiveModel {
        name: Set("Specific".to_string()),
        allowed_ips: Set(Some(serde_json::json!(["10.0.0.1"]))),
        incoming_to_user_prefix: Set(Some("8613".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create specific inbound trunk");
    let tenant = tenant::ActiveModel {
        name: Set("Tenant A".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");
    for trunk_id in [broad.id, specific.id] {
        tenant_trunk::ActiveModel {
            tenant_id: Set(tenant.id),
            sip_trunk_id: Set(trunk_id),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("link inbound trunk");
    }

    state.reload_runtime(&db).await.expect("reload runtime");
    let snapshot = state.routing.load();
    let mut callee = "86135551234".to_string();
    let inbound = snapshot
        .inbound_trunk_by_id(specific.id)
        .expect("an inbound trunk should match");
    assert_eq!(inbound.id, specific.id);
    assert!(inbound.matches("caller", &callee));
    inbound.rewrite_callee(&mut callee);
    assert_eq!(callee, "5551234");
}

#[test]
fn test_bills_dir_follows_recording_path() {
    let mut config = rustpbx::config::Config::default();
    assert_eq!(
        rustpbx::addons::wholesale::billing_service::bills_dir(&config),
        std::path::PathBuf::from("./config/recorders/wholesale_bills")
    );

    config.recording = Some(rustpbx::config::RecordingPolicy {
        path: Some("/data/recordings".to_string()),
        ..Default::default()
    });
    assert_eq!(
        rustpbx::addons::wholesale::billing_service::bills_dir(&config),
        std::path::PathBuf::from("/data/recordings/wholesale_bills")
    );
}

#[tokio::test]
async fn test_snapshot_seq_increments_on_reload() {
    let db = setup_db().await;
    let state = WholesaleState::new();
    assert_eq!(state.snapshot_seq(), 0);

    state.reload_runtime(&db).await.expect("first reload");
    assert_eq!(state.snapshot_seq(), 1);

    state.reload_runtime(&db).await.expect("second reload");
    assert_eq!(state.snapshot_seq(), 2);
}

#[tokio::test]
async fn test_add_peer_updates_memory_only() {
    let state = Arc::new(WholesaleState::new());
    *state.cluster_peers.lock().unwrap() = vec![rustpbx::config::ClusterPeer {
        addr: "10.0.0.1".to_string(),
        sip_port: 5060,
        ami_port: 5038,
    }];
    let initial_routing = state.routing.load_full();

    let response = api_add_peer(
        Extension(state.clone()),
        Json(rustpbx::config::ClusterPeer {
            addr: "10.0.0.2".to_string(),
            sip_port: 5060,
            ami_port: 5038,
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    assert!(Arc::ptr_eq(&initial_routing, &state.routing.load_full()));
    let peers = state.cluster_peers.lock().unwrap();
    assert_eq!(peers.len(), 2);
    assert_eq!(peers[1].addr, "10.0.0.2");
}

#[tokio::test]
async fn test_reload_runtime_from_db() {
    let db = setup_db().await;
    let state = WholesaleState::new();
    let sell_deck = rate_deck::ActiveModel {
        name: Set("Sell Deck".to_string()),
        r#type: Set(rate_deck::RateDeckType::Sell),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

    rate::ActiveModel {
        deck_id: Set(sell_deck.id),
        prefix: Set("123".to_string()),
        match_caller_prefix: Set(Some("8613".to_string())),
        rate: Set(0.5),
        min_duration: Set(60),
        increment: Set(1),
        remark: Set(Some("Test Rate".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create rate");

    let buy_deck = rate_deck::ActiveModel {
        name: Set("Buy Deck".to_string()),
        r#type: Set(rate_deck::RateDeckType::Buy),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck");

    rate::ActiveModel {
        deck_id: Set(buy_deck.id),
        prefix: Set("123".to_string()),
        match_caller_prefix: Set(None),
        rate: Set(0.2),
        min_duration: Set(60),
        increment: Set(1),
        remark: Set(Some("Buy Rate".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy rate");

    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier Trunk".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create carrier trunk");

    wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier_trunk.id),
        rate_deck_id: Set(Some(buy_deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create trunk config");

    let inbound_trunk = sip_trunk::ActiveModel {
        name: Set("Inbound Trunk".to_string()),
        allowed_ips: Set(Some(serde_json::json!(["10.0.0.1"]))),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create inbound trunk");

    let profile = routing_profile::ActiveModel {
        name: Set("Test Profile".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_trunk.id),
        priority: Set(1),
        weight: Set(10),
        match_callee_prefix: Set(Some("1".to_string())),
        rewrite_callee: Set(Some("2".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile item");

    let tenant = tenant::ActiveModel {
        name: Set("Tenant A".to_string()),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(sell_deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound_trunk.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("link tenant trunk");

    state
        .reload_runtime(&db)
        .await
        .expect("reload runtime from db");

    let routing_snapshot = state.routing.load_full();
    let mut callee = "123456".to_string();
    let inbound = routing_snapshot
        .inbound_trunk_by_id(inbound_trunk.id)
        .expect("inbound trunk should be embedded");
    assert!(inbound.matches("8613000000000", &callee));
    inbound.rewrite_callee(&mut callee);
    let tenant_node = routing_snapshot.tenant_for_inbound(inbound);
    assert_eq!(inbound.id, inbound_trunk.id);
    assert_eq!(tenant_node.id, tenant.id);
    assert_eq!(
        routing_snapshot
            .inbound_trunk_by_id(inbound_trunk.id)
            .map(|trunk| trunk.id),
        Some(inbound_trunk.id)
    );
    assert!(routing_snapshot.inbound_trunk_by_id(i64::MAX).is_none());

    let sell_rate_deck = tenant_node
        .rate_deck
        .map(|index| &routing_snapshot.rate_decks[index.0])
        .expect("sell rates should be embedded");
    let sell_rate = sell_rate_deck
        .find_best_rate("123456", Some("8613000000000"))
        .expect("sell rate should match");
    assert_eq!(sell_rate.prefix, "123");
    assert_eq!(sell_rate.match_caller_prefix, Some("8613".to_string()));
    assert_eq!(sell_rate.rate, 0.5);

    let no_rate = sell_rate_deck.find_best_rate("123456", Some(""));
    assert!(no_rate.is_none());

    let route_table = tenant_node
        .route_table
        .map(|index| &routing_snapshot.route_tables[index.0])
        .expect("routing profile should be embedded");
    assert_eq!(route_table.id, profile.id);
    let matched_routes = route_table.matching_routes("123456", "8613000000000");
    let route = matched_routes
        .iter()
        .copied()
        .find(|route| {
            routing_snapshot.outbound_trunks[route.outbound_trunk.0].id == carrier_trunk.id
        })
        .expect("outbound route should be embedded");
    let outbound_trunk = &routing_snapshot.outbound_trunks[route.outbound_trunk.0];
    assert_eq!(outbound_trunk.id, carrier_trunk.id);
    assert_eq!(outbound_trunk.rate_deck_id, Some(buy_deck.id));
    let mut rewritten_callee = "123456".to_string();
    route.apply_callee_rewrites(&mut rewritten_callee);
    assert_eq!(rewritten_callee, "123456");
    let buy_rate = outbound_trunk
        .rate_deck
        .map(|index| &routing_snapshot.rate_decks[index.0])
        .expect("buy rates should be embedded")
        .find_best_rate("123456", None)
        .expect("buy rate should match");
    assert_eq!(buy_rate.rate, 0.2);
    let candidates = [CarrierCandidate {
        route,
        trunk: outbound_trunk,
        buy_rate,
    }];
    assert_eq!(
        tenant_node
            .select_carrier(&candidates)
            .map(|candidate| candidate.trunk.id),
        Some(carrier_trunk.id)
    );
}

#[tokio::test]
async fn test_reload_runtime_allows_trunk_used_as_inbound_and_carrier() {
    let db = setup_db().await;
    let state = WholesaleState::new();
    let shared_trunk = sip_trunk::ActiveModel {
        name: Set("Shared Trunk".to_string()),
        allowed_ips: Set(Some(serde_json::json!(["10.0.0.1"]))),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create shared trunk");

    let buy_deck = rate_deck::ActiveModel {
        name: Set("Buy Deck".to_string()),
        r#type: Set(rate_deck::RateDeckType::Buy),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck");

    wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(shared_trunk.id),
        rate_deck_id: Set(Some(buy_deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create trunk config");

    let profile = routing_profile::ActiveModel {
        name: Set("Test Profile".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(shared_trunk.id),
        priority: Set(1),
        weight: Set(10),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile item");

    let tenant = tenant::ActiveModel {
        name: Set("Tenant A".to_string()),
        routing_profile_id: Set(Some(profile.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(shared_trunk.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("link tenant trunk");

    state
        .reload_runtime(&db)
        .await
        .expect("shared inbound/carrier trunk should not fail reload");
}

#[tokio::test]
async fn test_reload_runtime_keeps_unpriced_carrier_without_rate_deck() {
    let db = setup_db().await;
    let state = WholesaleState::new();
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier Trunk".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create carrier trunk");

    wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier_trunk.id),
        rate_deck_id: Set(None),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create empty trunk config");

    let profile = routing_profile::ActiveModel {
        name: Set("Test Profile".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_trunk.id),
        priority: Set(1),
        weight: Set(10),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile item");

    tenant::ActiveModel {
        name: Set("Tenant without priced carrier".to_string()),
        routing_profile_id: Set(Some(profile.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    state
        .reload_runtime(&db)
        .await
        .expect("carrier trunk without buy deck should remain loadable");

    let routing_snapshot = state.routing.load();
    let route_table = &routing_snapshot.route_tables[0];
    let matched_routes = route_table.matching_routes("123456", "caller");
    assert_eq!(matched_routes.len(), 1);
    let outbound_trunk = routing_snapshot
        .outbound_trunk_by_id(carrier_trunk.id)
        .expect("carrier trunk should be loaded");
    assert!(outbound_trunk.rate_deck.is_none());
}

#[test]
fn test_find_best_rate_prefers_matching_caller_prefix() {
    let matcher = RateMatcher::from(vec![
        RateConfig {
            id: 0,
            prefix: "1".to_string(),
            match_caller_prefix: None,
            rate: 0.01,
            min_duration: 60,
            increment: 60,
            remark: None,
        },
        RateConfig {
            id: 0,
            prefix: "1".to_string(),
            match_caller_prefix: Some("86".to_string()),
            rate: 0.02,
            min_duration: 60,
            increment: 60,
            remark: None,
        },
        RateConfig {
            id: 0,
            prefix: "1".to_string(),
            match_caller_prefix: Some("8613".to_string()),
            rate: 0.03,
            min_duration: 60,
            increment: 60,
            remark: None,
        },
        RateConfig {
            id: 0,
            prefix: "12".to_string(),
            match_caller_prefix: None,
            rate: 0.05,
            min_duration: 60,
            increment: 60,
            remark: None,
        },
        RateConfig {
            id: 0,
            prefix: "12".to_string(),
            match_caller_prefix: Some("852".to_string()),
            rate: 0.04,
            min_duration: 60,
            increment: 60,
            remark: None,
        },
    ]);

    let longer_callee_default = matcher
        .find_best_rate("123456", Some("8613000000000"))
        .expect("longer callee default rate");
    assert_eq!(longer_callee_default.prefix, "12");
    assert_eq!(longer_callee_default.rate, 0.05);

    let caller_specific_same_callee = matcher
        .find_best_rate("123456", Some("8521234"))
        .expect("caller-specific rate with same callee prefix");
    assert_eq!(caller_specific_same_callee.prefix, "12");
    assert_eq!(caller_specific_same_callee.rate, 0.04);

    let longer_callee_default = matcher
        .find_best_rate("123456", Some("441234"))
        .expect("longer callee default rate");
    assert_eq!(longer_callee_default.prefix, "12");
    assert_eq!(longer_callee_default.rate, 0.05);

    let no_caller = matcher
        .find_best_rate("123456", None)
        .expect("default rate");
    assert_eq!(no_caller.rate, 0.05);
}
