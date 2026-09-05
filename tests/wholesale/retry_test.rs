use rsipstack::dialog::invitation::InviteOption;
use rustpbx::addons::wholesale::data::{RateConfig, RateDeckConfig};
use rustpbx::addons::wholesale::migration::Migrator as WholesaleMigrator;
use rustpbx::addons::wholesale::models::{
    rate, rate_deck, routing_profile, routing_profile_item, tenant, tenant_trunk,
    wholesale_trunk_config,
};
use rustpbx::addons::wholesale::route::WholesaleRouteInvite;
use rustpbx::call::{RouteInvite, TrunkContext};
use rustpbx::models::{migration::Migrator as MainMigrator, sip_trunk};
use sea_orm::ActiveValue::Set;
use sea_orm::{Database, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;

async fn setup_test_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");

    MainMigrator::up(&db, None).await.expect("main migrations");
    WholesaleMigrator::up(&db, None)
        .await
        .expect("wholesale migrations");

    // 0. Routing Profile (ID 9999 to avoid file config collision)
    let profile = routing_profile::ActiveModel {
        id: Set(9999),
        name: Set("Test Profile".to_string()),
        enable_retry_policy: Set(true),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    };
    routing_profile::Entity::insert(profile)
        .exec(&db)
        .await
        .expect("insert routing profile");

    // 0. Rate Deck (Sell)
    let deck = rate_deck::ActiveModel {
        id: Set(9999),
        name: Set("Sell Deck".to_string()),
        r#type: Set(rate_deck::RateDeckType::Sell),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    };
    rate_deck::Entity::insert(deck)
        .exec(&db)
        .await
        .expect("insert rate deck");

    // 0. Rate
    let rate = rate::ActiveModel {
        deck_id: Set(9999),
        prefix: Set("".to_string()), // Match all
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    };
    rate::Entity::insert(rate)
        .exec(&db)
        .await
        .expect("insert rate");

    // 1. Insert Tenant
    let tenant = tenant::ActiveModel {
        id: Set(9999),
        name: Set("Test Tenant".to_string()),
        routing_profile_id: Set(Some(9999)), // We will use profile_id 9999
        rate_deck_id: Set(Some(9999)),       // Use Sell Deck
        credit_limit: Set(100.0),            // Ensure balance > 0 check passes
        balance: Set(100.0),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    };
    tenant::Entity::insert(tenant)
        .exec(&db)
        .await
        .expect("insert tenant");

    // 2. Insert Source Trunk (Caller)
    let inbound = sip_trunk::ActiveModel {
        id: Set(9100),
        name: Set("Source Trunk".to_string()),
        allowed_ips: Set(Some(serde_json::json!(["127.0.0.1"]))),
        ..Default::default()
    };
    sip_trunk::Entity::insert(inbound)
        .exec(&db)
        .await
        .expect("insert source trunk");

    // Link Tenant to Source Trunk via tenant_trunk
    let link = tenant_trunk::ActiveModel {
        tenant_id: Set(9999),
        sip_trunk_id: Set(9100),
        ..Default::default()
    };
    tenant_trunk::Entity::insert(link)
        .exec(&db)
        .await
        .expect("insert tenant trunk link");

    // 3. Insert Target Trunk
    let target_trunk = sip_trunk::ActiveModel {
        id: Set(9200),
        name: Set("Target Trunk".to_string()),
        sip_server: Set(Some("1.2.3.4".to_string())),
        ..Default::default()
    };
    sip_trunk::Entity::insert(target_trunk)
        .exec(&db)
        .await
        .expect("insert target trunk");

    // 4. Wholesale Trunk Config for Target
    let ws_config = wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(9200),
        rate_deck_id: Set(None),
        circuit_breaker_enabled: Set(false),
        cb_failure_threshold: Set(5),
        cb_open_duration_secs: Set(30),
        cb_half_open_probes: Set(1),
        cb_failure_codes: Set("503,408,504".to_string()),
        remark: Set(None),
        ringback: Set(None),
    };
    wholesale_trunk_config::Entity::insert(ws_config)
        .exec(&db)
        .await
        .expect("insert ws config");

    // 5. Keep the legacy retry configuration populated.
    let item = routing_profile_item::ActiveModel {
        profile_id: Set(9999),
        sip_trunk_id: Set(9200), // Targets Trunk 9200
        priority: Set(1),
        weight: Set(10),
        max_retries: Set(2),
        created_at: Set(chrono::Utc::now()),
        ..Default::default()
    };
    routing_profile_item::Entity::insert(item)
        .exec(&db)
        .await
        .expect("insert routing item");

    db
}

#[tokio::test]
async fn test_wholesale_route_ignores_retry_configuration() {
    // Enable tracing subscriber to see logs if needed
    // tracing_subscriber::fmt::try_init().ok();

    let db = setup_test_db().await;
    let state = Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new());

    // Persist the rate deck before rebuilding runtime state.
    crate::wholesale_helpers::insert_runtime_rate_deck(
        &db,
        RateDeckConfig {
            id: 9999,
            name: "Test Deck".to_string(),
            description: None,
            r#type: "Standard".to_string(),
            rates: vec![RateConfig {
                                prefix: "1".to_string(),
                match_caller_prefix: None,
                rate: 0.1,
                min_duration: 1,
                increment: 1,
                remark: None,
            }],
        },
    )
    .await;
    crate::wholesale_helpers::load_runtime_routing_profiles(&state, &db).await;

    let router = WholesaleRouteInvite {
        db: db.clone(),
        state: state.clone(),
    };

    // Construct a mock SIP request
    let invite_uri = rsipstack::sip::Uri::try_from("sip:1001@PBX_IP").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:2001@PBX_IP").unwrap();

    // Headers construction using available traits/methods
    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: rsipstack::sip::Headers::from(vec![
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(invite_uri.to_string())),
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(from_uri.to_string())),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("test-call-id")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
        ]),
        version: rsipstack::sip::Version::V2,
        body: vec![],
    };

    let dir = rustpbx::call::DialDirection::Outbound;
    let cookie = rustpbx::call::TransactionCookie::default();
    cookie.insert_extension(TrunkContext {
        id: Some(9100),
        name: "Source Trunk".to_string(),
        did_numbers: vec![],
    });

    let option = InviteOption {
        callee: invite_uri,
        caller: from_uri,
        destination: None,
        credential: None,
        ..Default::default()
    };

    // EXECUTE ROUTING
    let result = router.route_invite(option, &request, &dir, &cookie).await;

    // ASSERTIONS
    assert!(result.is_ok(), "Routing should succeed");
    let result = result.unwrap();

    let rustpbx::config::RouteResult::Forward(option, _) = result else {
        panic!("Legacy retry configuration must not produce a failover queue");
    };
    assert!(option.destination.is_some());
}
