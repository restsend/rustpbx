use rsipstack::dialog::invitation::InviteOption;
use rustpbx::addons::wholesale::{
    data::WholesaleState,
    migration::Migrator as WholesaleMigrator,
    models::{tenant, tenant_trunk},
    route::WholesaleRouteInvite,
};
use rustpbx::call::{DialDirection, RouteInvite, TransactionCookie, TrunkContext};
use rustpbx::config::RouteResult;
use rustpbx::models::{migration::Migrator as MainMigrator, sip_trunk};
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

fn source_cookie(inbound_trunk_id: i64) -> TransactionCookie {
    let cookie = TransactionCookie::default();
    cookie.insert_extension(TrunkContext {
        id: Some(inbound_trunk_id),
        name: String::new(),
        did_numbers: vec![],
    });
    cookie
}

#[tokio::test]
async fn test_acl_trunk_context_selects_source_trunk() {
    let db = setup_db().await;
    use chrono::Utc;
    use rustpbx::addons::wholesale::models::{
        rate, rate_deck, routing_profile, routing_profile_item,
    };

    let wrong_carrier = sip_trunk::ActiveModel {
        name: Set("Wrong Carrier".to_string()),
        direction: Set(sip_trunk::SipTrunkDirection::Outbound),
        sip_server: Set(Some("10.0.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wrong outbound");

    let right_carrier = sip_trunk::ActiveModel {
        name: Set("Right Carrier".to_string()),
        direction: Set(sip_trunk::SipTrunkDirection::Outbound),
        sip_server: Set(Some("10.0.0.2:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create right outbound");

    let wrong_source = sip_trunk::ActiveModel {
        name: Set("Wrong Source".to_string()),
        direction: Set(sip_trunk::SipTrunkDirection::Inbound),
        sip_server: Set(Some("1.2.4.4:5060".to_string())),
        allowed_ips: Set(Some(serde_json::json!(["1.2.4.4"]))),
        incoming_to_user_prefix: Set(Some("881".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wrong source trunk");

    let right_source = sip_trunk::ActiveModel {
        name: Set("Right Source".to_string()),
        direction: Set(sip_trunk::SipTrunkDirection::Inbound),
        sip_server: Set(Some("1.2.3.4:5080".to_string())),
        allowed_ips: Set(Some(serde_json::json!(["1.2.3.4"]))),
        incoming_to_user_prefix: Set(Some("88".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create right source trunk");

    let wrong_profile = routing_profile::ActiveModel {
        name: Set("Wrong Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wrong profile");

    let right_profile = routing_profile::ActiveModel {
        name: Set("Right Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create right profile");

    routing_profile_item::ActiveModel {
        profile_id: Set(wrong_profile.id),
        sip_trunk_id: Set(wrong_carrier.id),
        priority: Set(1),
        weight: Set(100),
        match_callee_prefix: Set(Some("234".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wrong profile item");

    routing_profile_item::ActiveModel {
        profile_id: Set(right_profile.id),
        sip_trunk_id: Set(right_carrier.id),
        priority: Set(1),
        weight: Set(100),
        match_callee_prefix: Set(Some("1234".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create right profile item");

    let wrong_deck = rate_deck::ActiveModel {
        name: Set("Wrong Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wrong deck");

    let right_deck = rate_deck::ActiveModel {
        name: Set("Right Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create right deck");

    rate::ActiveModel {
        deck_id: Set(wrong_deck.id),
        prefix: Set("234".to_string()),
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wrong rate");

    rate::ActiveModel {
        deck_id: Set(right_deck.id),
        prefix: Set("1234".to_string()),
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create right rate");

    let wrong_tenant = tenant::ActiveModel {
        name: Set("Wrong Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(wrong_profile.id)),
        rate_deck_id: Set(Some(wrong_deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wrong tenant");

    let right_tenant = tenant::ActiveModel {
        name: Set("Right Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(right_profile.id)),
        rate_deck_id: Set(Some(right_deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create right tenant");

    tenant_trunk::ActiveModel {
        tenant_id: Set(wrong_tenant.id),
        sip_trunk_id: Set(wrong_source.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("link wrong tenant trunk");

    tenant_trunk::ActiveModel {
        tenant_id: Set(right_tenant.id),
        sip_trunk_id: Set(right_source.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("link right tenant trunk");

    let state = Arc::new(WholesaleState::new());

    {
        use rustpbx::addons::wholesale::data::{RateConfig, RateDeckConfig};
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: wrong_deck.id,
                name: "Wrong Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                                        prefix: "234".to_string(),
                    match_caller_prefix: None,
                    rate: 0.01,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: right_deck.id,
                name: "Right Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                                        prefix: "1234".to_string(),
                    match_caller_prefix: None,
                    rate: 0.01,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }
    crate::wholesale_helpers::load_runtime_routing_profiles(&state, &db).await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: state.clone(),
    };

    let invite_uri = rsipstack::sip::Uri::try_from("sip:881234@127.0.0.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:caller@1.2.3.4").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:881234@127.0.0.1").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=123",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new(
                "real-source-exact-ip",
            )),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 1.2.4.4:5060;branch=z9hG4bK-wrong-via",
            )),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: Default::default(),
    };

    let option = InviteOption {
        callee: invite_uri,
        caller: from_uri,
        ..Default::default()
    };

    let cookie = TransactionCookie::default();
    cookie.insert_extension(TrunkContext {
        id: Some(right_source.id),
        name: right_source.name.clone(),
        did_numbers: vec![],
    });

    let result = route_invite
        .route_invite(option, &request, &DialDirection::Inbound, &cookie)
        .await
        .expect("route should succeed");

    match result {
        RouteResult::Forward(opt, _) => {
            assert_eq!(opt.destination.unwrap().addr.to_string(), "10.0.0.2:5060");
            assert_eq!(opt.callee.user().unwrap().to_string(), "1234");
        }
        _ => panic!("expected Forward through real-source trunk"),
    }
}

#[tokio::test]
async fn test_multi_trunk_context_and_stripping() {
    let db = setup_db().await;
    use chrono::Utc;
    use rustpbx::addons::wholesale::models::{
        rate, rate_deck, routing_profile, routing_profile_item,
    };

    // 1. Create Carrier Trunk (Destination)
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier Trunk".to_string()),
        sip_server: Set(Some("5.6.7.8:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound trunk");

    // 2. Create Source Trunk (Origin) with prefix 86138
    let inbound = sip_trunk::ActiveModel {
        name: Set("Trunk86138".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        allowed_ips: Set(Some(serde_json::json!(["1.2.3.4"]))),
        incoming_to_user_prefix: Set(Some("86138".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    // 3. Create Routing Profile
    let profile = routing_profile::ActiveModel {
        name: Set("Test Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    // 4. Link Profile to Carrier Trunk
    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_trunk.id),
        priority: Set(0),
        weight: Set(100),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile item");

    // 5. Create Rate Deck & Rate for the STRIPPED number
    // If we call 861380000, the stripped number is 0000.
    let deck = rate_deck::ActiveModel {
        name: Set("Test Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

    rate::ActiveModel {
        deck_id: Set(deck.id),
        prefix: Set("0000".to_string()), // Matches the stripped number
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create rate");

    // 6. Create Tenant
    let tenant = tenant::ActiveModel {
        name: Set("Test Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    // 7. Link Tenant to Source Trunk
    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk link");

    // 8. Setup WholesaleRouteInvite
    let state = Arc::new(WholesaleState::new());

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: state.clone(),
    };

    // Persist rates before rebuilding runtime state.
    {
        use rustpbx::addons::wholesale::data::{RateConfig, RateDeckConfig};
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Test Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                                        prefix: "0000".to_string(),
                    match_caller_prefix: None,
                    rate: 0.01,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }
    crate::wholesale_helpers::load_runtime_routing_profiles(&route_invite.state, &db).await;

    // 9. Create INVITE for 861380000
    let invite_uri = rsipstack::sip::Uri::try_from("sip:861380000@127.0.0.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:caller@1.2.3.4").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:861380000@127.0.0.1").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=123",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("callid-strip")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 1.2.3.4:5060;branch=z9hG4bK-strip",
            )),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: Default::default(),
    };

    let option = InviteOption {
        callee: invite_uri,
        caller: from_uri,
        ..Default::default()
    };

    // 10. Setup source address metadata
    let cookie = source_cookie(inbound.id);

    // 10. Run Route
    let result = route_invite
        .route_invite(option.clone(), &request, &DialDirection::Inbound, &cookie)
        .await
        .expect("route invite success");

    // 11. Assert Forward (meaning prefix was stripped and rate matched)
    match result {
        RouteResult::Forward(opt, _) => {
            // Check if destination is set to outbound trunk
            assert!(opt.destination.is_some());
            assert_eq!(opt.destination.unwrap().addr.to_string(), "5.6.7.8:5060");

            // Check if callee in Forward is the STRIPPED one (0000)
            // Note: route_wholesale constructs new_uri_str with new_callee
            assert_eq!(opt.callee.user().unwrap().to_string(), "0000");
        }
        RouteResult::Abort(_, msg) => panic!("Expected Forward, got Abort: {:?}", msg),
        _ => panic!("Expected Forward result"),
    }

    // --- Scenario 1: One tenant, multiple trunks, same IP, different prefixes ---

    // 12. Create another trunk for the SAME tenant with prefix 86155
    let source_trunk2 = sip_trunk::ActiveModel {
        name: Set("Trunk86155".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        allowed_ips: Set(Some(serde_json::json!(["1.2.3.4"]))),
        incoming_to_user_prefix: Set(Some("86155".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk 2");

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(source_trunk2.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("link tenant to trunk 2");

    // Rebuild the wholesale snapshot after adding the second inbound trunk.
    crate::wholesale_helpers::load_runtime_routing_profiles(&route_invite.state, &db).await;

    // 13. Create INVITE for 861551111
    let invite_uri2 = rsipstack::sip::Uri::try_from("sip:861551111@127.0.0.1").unwrap();
    let to_uri2 = rsipstack::sip::Uri::try_from("sip:861551111@127.0.0.1").unwrap();

    let mut headers2 = request.headers.clone();
    // Replace To header instead of pushing
    headers2.retain(|h| !matches!(h, rsipstack::sip::Header::To(_)));
    headers2.push(rsipstack::sip::Header::To(
        rsipstack::sip::headers::To::new(format!("<{}>", to_uri2)),
    ));

    headers2.retain(|h| !matches!(h, rsipstack::sip::Header::CallId(_)));
    headers2.push(rsipstack::sip::Header::CallId(
        rsipstack::sip::headers::CallId::new("callid-strip-2"),
    ));

    let request2 = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri2.clone(),
        headers: headers2,
        version: rsipstack::sip::Version::V2,
        body: Default::default(),
    };

    let mut option2 = option.clone();
    option2.callee = invite_uri2;

    // Replace the persisted deck with a rate for 1111.
    {
        use rustpbx::addons::wholesale::data::{RateConfig, RateDeckConfig};
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Test Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                                        prefix: "1111".to_string(),
                    match_caller_prefix: None,
                    rate: 0.01,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }

    let cookie2 = source_cookie(source_trunk2.id);

    // 14. Run Route
    let _ = route_invite
        .route_invite(option2, &request2, &DialDirection::Inbound, &cookie2)
        .await
        .expect("route invite success");

    // The ACL-selected trunk ID determines which inbound prefix is applied.
}

#[tokio::test]
async fn test_single_trunk_returns_forward() {
    let db = setup_db().await;
    use chrono::Utc;
    use rustpbx::addons::wholesale::models::{
        rate, rate_deck, routing_profile, routing_profile_item,
    };

    // Create a single outbound trunk
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier A".to_string()),
        sip_server: Set(Some("10.0.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create source trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        allowed_ips: Set(Some(serde_json::json!(["1.2.3.4"]))),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create routing profile
    let profile = routing_profile::ActiveModel {
        name: Set("Single Trunk Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Add ONLY ONE trunk to profile
    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_trunk.id),
        is_active: Set(true),
        priority: Set(1),
        weight: Set(100),
        match_callee_prefix: Set(Some("1".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create rate deck and rate
    let deck = rate_deck::ActiveModel {
        name: Set("Test Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    rate::ActiveModel {
        deck_id: Set(deck.id),
        prefix: Set("1".to_string()),
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create tenant
    let tenant = tenant::ActiveModel {
        name: Set("Test Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Setup route invite
    let state = Arc::new(WholesaleState::new());

    // Persist rates before rebuilding runtime state.
    {
        use rustpbx::addons::wholesale::data::{RateConfig, RateDeckConfig};
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Test Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                                        prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.01,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }
    crate::wholesale_helpers::load_runtime_routing_profiles(&state, &db).await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: state.clone(),
    };

    // Create INVITE
    let invite_uri = rsipstack::sip::Uri::try_from("sip:1234@127.0.0.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:caller@1.2.3.4").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:1234@127.0.0.1").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=123",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("single-trunk")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 1.2.3.4:5060;branch=z9hG4bK-single",
            )),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: Default::default(),
    };

    let option = InviteOption {
        callee: invite_uri,
        caller: from_uri,
        ..Default::default()
    };

    // Setup source address metadata
    let cookie = source_cookie(inbound.id);

    // Route the call
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Inbound, &cookie)
        .await
        .expect("route should succeed");

    let RouteResult::Forward(option, _) = result else {
        panic!("Expected Forward result");
    };
    assert_eq!(
        option.destination.unwrap().addr.to_string(),
        "10.0.0.1:5060"
    );
}

#[tokio::test]
async fn test_multiple_trunks_selects_one_carrier() {
    let db = setup_db().await;
    use chrono::Utc;
    use rustpbx::addons::wholesale::models::{
        rate, rate_deck, routing_profile, routing_profile_item,
    };

    // Create THREE outbound trunks with different priorities
    let carrier_a = sip_trunk::ActiveModel {
        name: Set("Carrier A".to_string()),
        sip_server: Set(Some("10.0.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    let carrier_b = sip_trunk::ActiveModel {
        name: Set("Carrier B".to_string()),
        sip_server: Set(Some("10.0.0.2:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    let carrier_c = sip_trunk::ActiveModel {
        name: Set("Carrier C".to_string()),
        sip_server: Set(Some("10.0.0.3:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create source trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        allowed_ips: Set(Some(serde_json::json!(["1.2.3.4"]))),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create routing profile
    let profile = routing_profile::ActiveModel {
        name: Set("Multi Trunk Profile".to_string()),
        enable_retry_policy: Set(true),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Add three same-priority trunks. Weight makes Carrier A deterministic.
    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_a.id),
        priority: Set(1),
        weight: Set(100),
        match_callee_prefix: Set(Some("1".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_b.id),
        priority: Set(1),
        weight: Set(0),
        match_callee_prefix: Set(Some("1".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_c.id),
        priority: Set(1),
        weight: Set(0),
        match_callee_prefix: Set(Some("1".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create rate deck and rate
    let deck = rate_deck::ActiveModel {
        name: Set("Test Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    rate::ActiveModel {
        deck_id: Set(deck.id),
        prefix: Set("1".to_string()),
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create tenant
    let tenant = tenant::ActiveModel {
        name: Set("Test Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Setup route invite
    let state = Arc::new(WholesaleState::new());

    // Persist rates before rebuilding runtime state.
    {
        use rustpbx::addons::wholesale::data::{RateConfig, RateDeckConfig};
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Test Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                                        prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.01,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }
    crate::wholesale_helpers::load_runtime_routing_profiles(&state, &db).await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: state.clone(),
    };

    // Create INVITE
    let invite_uri = rsipstack::sip::Uri::try_from("sip:1234@127.0.0.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:caller@1.2.3.4").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:1234@127.0.0.1").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=123",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("multi-trunk")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 1.2.3.4:5060;branch=z9hG4bK-multi",
            )),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: Default::default(),
    };

    let option = InviteOption {
        callee: invite_uri,
        caller: from_uri,
        ..Default::default()
    };

    // Setup source address metadata
    let cookie = source_cookie(inbound.id);

    // Route the call
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Inbound, &cookie)
        .await
        .expect("route should succeed");

    // Retry configuration remains stored, but routing selects one carrier.
    match result {
        RouteResult::Forward(option, _) => {
            assert_eq!(
                option.destination.unwrap().addr.to_string(),
                "10.0.0.1:5060"
            );
        }
        RouteResult::Abort(code, msg) => {
            panic!("Expected Forward, got Abort: {} {:?}", code, msg);
        }
        _ => panic!("Wholesale routing must return a single carrier"),
    }
}

#[tokio::test]
async fn test_abort_when_no_trunks_have_valid_rates() {
    let db = setup_db().await;
    use chrono::Utc;
    use rustpbx::addons::wholesale::models::{
        rate, rate_deck, routing_profile, routing_profile_item,
    };

    // Create TWO outbound trunks - both will fail due to no matching rate
    let carrier_a = sip_trunk::ActiveModel {
        name: Set("Carrier A".to_string()),
        sip_server: Set(Some("10.0.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    let carrier_b = sip_trunk::ActiveModel {
        name: Set("Carrier B".to_string()),
        sip_server: Set(Some("10.0.0.2:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create source trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        allowed_ips: Set(Some(serde_json::json!(["1.2.3.4"]))),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create routing profile
    let profile = routing_profile::ActiveModel {
        name: Set("Failover Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Add both trunks - both match prefix "99"
    // But we'll only have rate for "999" (3 digits), not "99" (2 digits)
    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_a.id),
        priority: Set(1), // Higher priority
        weight: Set(100),
        match_callee_prefix: Set(Some("99".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    routing_profile_item::ActiveModel {
        profile_id: Set(profile.id),
        sip_trunk_id: Set(carrier_b.id),
        priority: Set(2), // Lower priority
        weight: Set(100),
        match_callee_prefix: Set(Some("99".to_string())),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create rate deck with rate ONLY for 999xxx, not 99xxx
    let deck = rate_deck::ActiveModel {
        name: Set("Test Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Add rate for "999" - this will NOT match "99123" (only 2 digits match)
    rate::ActiveModel {
        deck_id: Set(deck.id),
        prefix: Set("999".to_string()),
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Create tenant
    let tenant = tenant::ActiveModel {
        name: Set("Test Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Setup route invite
    let state = Arc::new(WholesaleState::new());

    // Persist only the "999" rate before rebuilding runtime state.
    // Calling 99123 will NOT match this rate (need exact or longer prefix match)
    {
        use rustpbx::addons::wholesale::data::{RateConfig, RateDeckConfig};
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Test Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                                        prefix: "999".to_string(), // This will match 999xxx but NOT 99xxx
                    match_caller_prefix: None,
                    rate: 0.01,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }
    crate::wholesale_helpers::load_runtime_routing_profiles(&state, &db).await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: state.clone(),
    };

    // Create INVITE for 99123 (NOT 999xxx)
    // This will NOT match rate "999", so first trunk will be skipped
    let invite_uri = rsipstack::sip::Uri::try_from("sip:99123@127.0.0.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:caller@1.2.3.4").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:99123@127.0.0.1").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=123",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("no-rate-test")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 1.2.3.4:5060;branch=z9hG4bK-no-rate",
            )),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: Default::default(),
    };

    let option = InviteOption {
        callee: invite_uri,
        caller: from_uri,
        ..Default::default()
    };

    // Setup source address metadata
    let cookie = source_cookie(inbound.id);

    // Route the call
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Inbound, &cookie)
        .await
        .expect("route should succeed");

    // Assert: Since 99123 does NOT match rate prefix "999", BOTH trunks will be skipped
    // This should return Abort because no trunks have valid rates
    match result {
        RouteResult::Abort(code, msg) => {
            assert_eq!(code, rsipstack::sip::StatusCode::ServiceUnavailable);
            assert!(msg.is_some());
            println!("✓ Correctly returned Abort when no trunks have valid rates");
        }
        RouteResult::Forward(opt, _) => {
            panic!("Expected Abort, got Forward to {:?}", opt.destination);
        }
        _ => panic!("Expected Abort result"),
    }
}
