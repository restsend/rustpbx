use chrono::Utc;
use rsipstack::dialog::invitation::InviteOption;
use rustpbx::addons::wholesale::{
    data::{
        CallerNumberPool, OutboundTrunkIndex, RateConfig, RateDeckConfig, Route, RouteTable,
        RoutingProfileConfig, RoutingProfileItemConfig, WholesaleState,
    },
    matching::{RewriteRule, prepare_time_window},
    migration::Migrator as WholesaleMigrator,
    models::{rate, rate_deck, routing_profile, tenant, tenant_trunk},
    route::{WholesaleBillingContext, WholesaleRouteInvite},
};
use rustpbx::call::{
    DialDirection, OutboundTrunkContext, RouteInvite, TransactionCookie, TrunkContext,
};
use rustpbx::config::RouteResult;
use rustpbx::models::{migration::Migrator as MainMigrator, sip_trunk};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use std::sync::Arc;

fn test_route_table(profile: RoutingProfileConfig) -> RouteTable {
    let mut items: Vec<_> = profile
        .items
        .into_iter()
        .filter(|item| item.is_active)
        .collect();
    items.sort_by_key(|item| item.priority);
    let mut trie = rustpbx::addons::wholesale::trie::PrefixTrie::new();
    for item in items {
        let match_callee_country_id = item
            .match_callee_country
            .as_deref()
            .and_then(|country| country.parse::<phonenumber::country::Id>().ok());
        let match_caller_country_id = item
            .match_caller_country
            .as_deref()
            .and_then(|country| country.parse::<phonenumber::country::Id>().ok());
        let rewrite_callee = item
            .rewrite_callee
            .as_deref()
            .and_then(|rule| RewriteRule::try_from(rule).ok());
        let caller_uses_pool = item.caller_selection_policy.as_deref() == Some("pool");
        let rewrite_caller = item
            .rewrite_caller
            .as_deref()
            .and_then(|rule| RewriteRule::try_from(rule).ok());
        let prepared_time_window = prepare_time_window(
            item.time_window_start.as_deref(),
            item.time_window_end.as_deref(),
            item.time_window_days.as_deref(),
            item.time_window_timezone,
        );
        let prefix = item.match_callee_prefix.clone().unwrap_or_default();
        trie.push(
            &prefix,
            Route {
                id: item.id,
                sip_trunk_id: item.sip_trunk_id,
                outbound_trunk: OutboundTrunkIndex(0),
                priority: item.priority,
                weight: item.weight,
                match_caller_prefix: item.match_caller_prefix,
                match_callee_country_id,
                match_caller_country_id,
                rewrite_callee,
                rewrite_caller,
                caller_number_pool: if caller_uses_pool {
                    item.caller_number_pool
                        .as_deref()
                        .and_then(CallerNumberPool::from_config)
                } else {
                    None
                },
                strip_digits: item.strip_digits,
                prepend_digits: item.prepend_digits,
                prepared_time_window,
            },
        );
    }

    RouteTable {
        id: profile.id,
        name: profile.name,
        trie,
    }
}

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

const TEST_SOURCE_IP: &str = "127.0.0.1";

fn test_source_allowed_ips() -> serde_json::Value {
    serde_json::json!([TEST_SOURCE_IP])
}

fn source_cookie(inbound_trunk_id: Option<i64>) -> TransactionCookie {
    let cookie = TransactionCookie::default();
    if let Some(inbound_trunk_id) = inbound_trunk_id {
        cookie.insert_extension(TrunkContext {
            id: Some(inbound_trunk_id),
            name: String::new(),
            did_numbers: vec![],
        });
    }
    cookie
}

async fn load_test_routing_profile(
    state: &Arc<WholesaleState>,
    db: &DatabaseConnection,
    profile: &routing_profile::Model,
    items: Vec<RoutingProfileItemConfig>,
) {
    crate::wholesale_helpers::insert_runtime_routing_profile_config(
        db,
        RoutingProfileConfig {
            id: profile.id,
            name: profile.name.clone(),
            description: profile.description.clone(),
            enable_retry_policy: profile.enable_retry_policy,
            retry_codes: profile.retry_codes.clone(),
            max_failover_items: profile.max_failover_items,
            no_trying_timeout_ms: profile.no_trying_timeout_ms,
            items,
        },
    )
    .await;
    crate::wholesale_helpers::load_runtime_routing_profiles(state, db).await;
}

#[tokio::test]
async fn test_non_wholesale_call_is_not_handled() {
    let db = setup_db().await;
    let route_invite = WholesaleRouteInvite {
        db,
        state: Arc::new(WholesaleState::new()),
    };
    let cookie = source_cookie(None);
    let (request, option) = make_invite("1000", "caller");

    let result = route_invite
        .route_invite(option, &request, &DialDirection::Inbound, &cookie)
        .await
        .expect("non-wholesale route should be handled by the main resolver");

    assert!(matches!(result, RouteResult::NotHandled(_, None)));
}

#[tokio::test]
async fn test_wholesale_route_invite_with_source_ip() {
    let db = setup_db().await;

    // 1. Create Carrier Trunk (Destination)
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier Trunk".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        metadata: Set(Some(serde_json::json!({
            "sbc": { "audio_codecs": ["PCMU"] }
        }))),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound trunk");

    // 2. Create Source Trunk (Origin)
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source Trunk".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
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

    // 5. Create Rate Deck & Rate
    let deck = rate_deck::ActiveModel {
        name: Set("Test Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

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

    // 7. Setup WholesaleRouteInvite
    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new()),
    };

    // Persist rates before rebuilding runtime state.
    {
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
    load_test_routing_profile(
        &route_invite.state,
        &db,
        &profile,
        vec![RoutingProfileItemConfig {
            sip_trunk_id: carrier_trunk.id,
            priority: 0,
            weight: 100,
            rewrite_callee: Some("s/^123/456/".to_string()),
            ..Default::default()
        }],
    )
    .await;

    // 8. Create INVITE with source address metadata
    let cookie = source_cookie(Some(inbound.id));

    let invite_uri = rsipstack::sip::Uri::try_from("sip:123456@192.168.1.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:test_user@198.51.100.10").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:123456@192.168.1.1").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=123",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("callid")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bK",
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

    // 9. Run Route (Outbound direction, which was previously failing)
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route invite success");

    // 10. Assert
    match result {
        RouteResult::Forward(opt, Some(hints)) => {
            // Check if destination is set to outbound trunk
            assert!(opt.destination.is_some());

            let cookie_inbound = cookie
                .get_extension::<TrunkContext>()
                .expect("inbound trunk context in cookie");
            assert_eq!(cookie_inbound.id, Some(inbound.id));
            let cookie_outbound = cookie
                .get_extension::<OutboundTrunkContext>()
                .expect("outbound trunk context in cookie");
            let hints_outbound = hints
                .extensions
                .get::<OutboundTrunkContext>()
                .expect("outbound trunk context in dialplan hints");
            assert_eq!(hints_outbound, &cookie_outbound);
            assert_eq!(hints.allow_codecs, Some(vec!["PCMU".to_string()]));
            let dest = opt.destination.unwrap();
            assert_eq!(dest.addr.to_string(), "1.2.3.4:5060");

            // Check if R-URI host is updated to outbound trunk
            let callee_host = opt.callee.host_with_port.to_string();
            assert_eq!(callee_host, "1.2.3.4:5060");
            assert_eq!(opt.caller.to_string(), "sip:test_user@192.168.1.1");
            assert_eq!(opt.callee.to_string(), "sip:456456@1.2.3.4:5060");
        }
        _ => panic!("Expected Forward result"),
    }
}

#[tokio::test]
async fn test_wholesale_route_country_match() {
    let db = setup_db().await;

    let wrong_country_trunk = sip_trunk::ActiveModel {
        name: Set("Wrong Country Trunk".to_string()),
        sip_server: Set(Some("3.3.3.3:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wrong country trunk");

    let cn_trunk = sip_trunk::ActiveModel {
        name: Set("CN Trunk".to_string()),
        sip_server: Set(Some("4.4.4.4:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create cn trunk");

    let inbound = sip_trunk::ActiveModel {
        name: Set("Source Trunk".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    let profile = routing_profile::ActiveModel {
        name: Set("Country Match Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    let deck = rate_deck::ActiveModel {
        name: Set("Country Match Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

    rate::ActiveModel {
        deck_id: Set(deck.id),
        prefix: Set("+86".to_string()),
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create rate");

    let tenant = tenant::ActiveModel {
        name: Set("Country Match Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk link");

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new()),
    };

    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Country Match Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                    prefix: "+86".to_string(),
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
    load_test_routing_profile(
        &route_invite.state,
        &db,
        &profile,
        vec![
            RoutingProfileItemConfig {
                sip_trunk_id: wrong_country_trunk.id,
                priority: 0,
                weight: 100,
                match_callee_prefix: Some("+86".to_string()),
                match_callee_country: Some("US".to_string()),
                match_caller_country: Some("US".to_string()),
                ..Default::default()
            },
            RoutingProfileItemConfig {
                sip_trunk_id: cn_trunk.id,
                priority: 1,
                weight: 100,
                match_callee_prefix: Some("+86".to_string()),
                match_callee_country: Some("CN".to_string()),
                match_caller_country: Some("US".to_string()),
                ..Default::default()
            },
        ],
    )
    .await;

    let cookie = source_cookie(Some(inbound.id));

    let invite_uri = rsipstack::sip::Uri::try_from("sip:+8613800000000@192.168.1.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:+14155552671@192.168.1.1").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:+8613800000000@192.168.1.1").unwrap();

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
                "callid_country_match",
            )),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bKCountry",
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

    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route invite success");

    match result {
        RouteResult::Forward(opt, _) => {
            let dest = opt.destination.unwrap();
            assert_eq!(dest.addr.to_string(), "4.4.4.4:5060");
        }
        RouteResult::Abort(code, msg) => panic!(
            "Expected Forward result for country match, got Abort: {:?} {:?}",
            code, msg
        ),
        _ => panic!("Expected Forward result for country match"),
    }
}

#[tokio::test]
async fn test_wholesale_route_priority_wins_after_prefix_match() {
    let db = setup_db().await;

    // 1. Create Carrier Trunks
    let carrier_trunk_catch_all = sip_trunk::ActiveModel {
        name: Set("Catch All Trunk".to_string()),
        sip_server: Set(Some("1.1.1.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create catch all trunk");

    let carrier_trunk_specific = sip_trunk::ActiveModel {
        name: Set("Specific Trunk".to_string()),
        sip_server: Set(Some("2.2.2.2:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create specific trunk");

    // 2. Create Source Trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source Trunk".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
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

    // 4.5 Create Rate Deck & Rates
    let deck = rate_deck::ActiveModel {
        name: Set("Test Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

    // Rate for 123456
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
    .expect("create rate 1");

    // Rate for 567890
    rate::ActiveModel {
        deck_id: Set(deck.id),
        prefix: Set("5".to_string()),
        rate: Set(0.01),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create rate 5");

    // 5. Create Tenant
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

    // 6. Link Tenant to Source Trunk
    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk link");

    // 7. Setup WholesaleRouteInvite
    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new()),
    };

    // Persist rates before rebuilding runtime state.
    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Test Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![
                    RateConfig {
                        prefix: "1".to_string(),
                        match_caller_prefix: None,
                        rate: 0.01,
                        min_duration: 60,
                        increment: 60,
                        remark: None,
                    },
                    RateConfig {
                        prefix: "5".to_string(),
                        match_caller_prefix: None,
                        rate: 0.01,
                        min_duration: 60,
                        increment: 60,
                        remark: None,
                    },
                ],
            },
        )
        .await;
    }
    load_test_routing_profile(
        &route_invite.state,
        &db,
        &profile,
        vec![
            RoutingProfileItemConfig {
                sip_trunk_id: carrier_trunk_catch_all.id,
                priority: 1,
                weight: 100,
                match_callee_prefix: None,
                ..Default::default()
            },
            RoutingProfileItemConfig {
                sip_trunk_id: carrier_trunk_specific.id,
                priority: 10,
                weight: 100,
                match_callee_prefix: Some("1234".to_string()),
                ..Default::default()
            },
        ],
    )
    .await;

    // 8. Setup source address metadata
    let cookie = source_cookie(Some(inbound.id));

    // --- Test Case 1: Specific Match (123456) ---
    let invite_uri = rsipstack::sip::Uri::try_from("sip:123456@192.168.1.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:test_user@192.168.1.1").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:123456@192.168.1.1").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=123",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("callid1")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bK1",
            )),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: Default::default(),
    };

    let option = InviteOption {
        callee: invite_uri.clone(),
        caller: from_uri.clone(),
        ..Default::default()
    };

    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route invite success");

    match result {
        RouteResult::Forward(opt, _) => {
            let dest = opt.destination.unwrap();
            // Both items match, but the catch-all item has better priority.
            assert_eq!(
                dest.addr.to_string(),
                "1.1.1.1:5060",
                "Priority should win after prefix matching"
            );
        }
        RouteResult::Abort(code, msg) => panic!(
            "Expected Forward result for specific match, got Abort: {:?} {:?}",
            code, msg
        ),
        _ => panic!("Expected Forward result for specific match, got other"),
    }

    // --- Test Case 2: Catch-all Match (567890) ---
    let invite_uri = rsipstack::sip::Uri::try_from("sip:567890@192.168.1.1").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:567890@192.168.1.1").unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=123",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("callid2")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("2 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bK2",
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

    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route invite success");

    match result {
        RouteResult::Forward(opt, _) => {
            let dest = opt.destination.unwrap();
            // Should match catch-all trunk (1.1.1.1) because "567890" does not start with "1234"
            assert_eq!(
                dest.addr.to_string(),
                "1.1.1.1:5060",
                "Should match catch-all trunk"
            );
        }
        _ => panic!("Expected Forward result for catch-all match"),
    }
}

#[test]
fn test_runtime_route_matching_keeps_only_best_priority_group() {
    let profile = test_route_table(RoutingProfileConfig {
        id: 1,
        name: "Best Priority Profile".to_string(),
        description: None,
        enable_retry_policy: false,
        retry_codes: None,
        max_failover_items: 10,
        no_trying_timeout_ms: None,
        items: vec![
            RoutingProfileItemConfig {
                id: 0,
                sip_trunk_id: 5,
                is_active: false,
                priority: 0,
                weight: 100,
                match_callee_prefix: Some("8613".to_string()),
                ..Default::default()
            },
            RoutingProfileItemConfig {
                id: 1,
                sip_trunk_id: 10,
                priority: 1,
                weight: 100,
                match_callee_prefix: Some("86".to_string()),
                ..Default::default()
            },
            RoutingProfileItemConfig {
                id: 2,
                sip_trunk_id: 20,
                priority: 1,
                weight: 50,
                match_callee_prefix: Some("8613".to_string()),
                ..Default::default()
            },
            RoutingProfileItemConfig {
                id: 3,
                sip_trunk_id: 30,
                priority: 10,
                weight: 100,
                match_callee_prefix: Some("8613".to_string()),
                ..Default::default()
            },
        ],
    });

    let matched = profile.matching_routes("86131234567", "caller");
    let mut matched_ids: Vec<i64> = matched.into_iter().map(|item| item.id).collect();
    matched_ids.sort_unstable();

    assert_eq!(matched_ids, vec![1, 2]);
}

#[test]
fn test_runtime_route_matching_replaces_worse_priority_seen_first() {
    let profile = test_route_table(RoutingProfileConfig {
        id: 1,
        name: "Priority Replacement Profile".to_string(),
        description: None,
        enable_retry_policy: false,
        retry_codes: None,
        max_failover_items: 10,
        no_trying_timeout_ms: None,
        items: vec![
            RoutingProfileItemConfig {
                id: 1,
                sip_trunk_id: 10,
                priority: 10,
                weight: 100,
                match_callee_prefix: Some("86".to_string()),
                ..Default::default()
            },
            RoutingProfileItemConfig {
                id: 2,
                sip_trunk_id: 20,
                priority: 1,
                weight: 100,
                match_callee_prefix: Some("8613".to_string()),
                ..Default::default()
            },
        ],
    });

    let matched = profile.matching_routes("86131234567", "caller");
    let matched_ids: Vec<i64> = matched.into_iter().map(|item| item.id).collect();

    assert_eq!(matched_ids, vec![2]);
}

#[test]
fn test_route_rewrite_applies_all_changes_in_order() {
    let profile = test_route_table(RoutingProfileConfig {
        id: 1,
        name: "Rewrite Profile".to_string(),
        description: None,
        enable_retry_policy: false,
        retry_codes: None,
        max_failover_items: 0,
        no_trying_timeout_ms: None,
        items: vec![RoutingProfileItemConfig {
            id: 10,
            sip_trunk_id: 20,
            priority: 1,
            weight: 100,
            strip_digits: Some(2),
            prepend_digits: Some("88".to_string()),
            rewrite_callee: Some("s/^88/99/".to_string()),
            rewrite_caller: Some("s/^00/11/".to_string()),
            ..Default::default()
        }],
    });
    let route = profile.matching_routes("121234", "00123")[0];
    let original_caller = "00123".to_string();
    let original_callee = "121234".to_string();
    let mut caller = original_caller.clone();
    let mut callee = original_callee.clone();
    route.apply_callee_rewrites(&mut callee);
    route.apply_caller_rewrite(&mut caller);

    assert_eq!(original_caller, "00123");
    assert_eq!(original_callee, "121234");
    assert_eq!(caller, "11123");
    assert_eq!(callee, "991234");
}

#[test]
fn test_caller_pool_overrides_rewrite_and_empty_pool_falls_back() {
    let profile = test_route_table(RoutingProfileConfig {
        id: 1,
        name: "Caller Profile".to_string(),
        description: None,
        enable_retry_policy: false,
        retry_codes: None,
        max_failover_items: 0,
        no_trying_timeout_ms: None,
        items: vec![
            RoutingProfileItemConfig {
                id: 10,
                sip_trunk_id: 20,
                priority: 1,
                weight: 100,
                match_callee_prefix: Some("1".to_string()),
                rewrite_caller: Some("s/^00/11/".to_string()),
                caller_selection_policy: Some("pool".to_string()),
                caller_number_pool: Some("2001".to_string()),
                ..Default::default()
            },
            RoutingProfileItemConfig {
                id: 11,
                sip_trunk_id: 20,
                priority: 1,
                weight: 100,
                match_callee_prefix: Some("2".to_string()),
                rewrite_caller: Some("s/^00/11/".to_string()),
                caller_selection_policy: Some("pool".to_string()),
                caller_number_pool: Some("\n".to_string()),
                ..Default::default()
            },
        ],
    });

    let mut caller = "00123".to_string();
    profile.matching_routes("123", &caller)[0].apply_caller_rewrite(&mut caller);
    assert_eq!(caller, "2001");

    let mut caller = "00123".to_string();
    profile.matching_routes("234", &caller)[0].apply_caller_rewrite(&mut caller);
    assert_eq!(caller, "11123");
}

#[tokio::test]
async fn test_config_trie_selects_from_matching_callee_prefixes() {
    let db = setup_db().await;

    let short_prefix_trunk = sip_trunk::ActiveModel {
        name: Set("Short Prefix Trunk".to_string()),
        sip_server: Set(Some("10.10.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create short prefix trunk");

    let long_prefix_trunk = sip_trunk::ActiveModel {
        name: Set("Long Prefix Trunk".to_string()),
        sip_server: Set(Some("10.10.0.2:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create long prefix trunk");

    let inbound = sip_trunk::ActiveModel {
        name: Set("Config Trie Source".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    let profile = routing_profile::ActiveModel {
        name: Set("Config Trie Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    let deck = rate_deck::ActiveModel {
        name: Set("Config Trie Sell Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

    let tenant = tenant::ActiveModel {
        name: Set("Config Trie Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk link");

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new()),
    };

    let short_item = RoutingProfileItemConfig {
        id: 1,
        sip_trunk_id: short_prefix_trunk.id,
        priority: 1,
        weight: 100,
        match_callee_prefix: Some("1".to_string()),
        ..Default::default()
    };
    let long_item = RoutingProfileItemConfig {
        id: 2,
        sip_trunk_id: long_prefix_trunk.id,
        priority: 1,
        weight: 100,
        match_callee_prefix: Some("1212".to_string()),
        ..Default::default()
    };

    {
        crate::wholesale_helpers::insert_runtime_routing_profile_config(
            &db,
            RoutingProfileConfig {
                id: profile.id,
                name: "Config Trie Profile".to_string(),
                description: None,
                enable_retry_policy: true,
                retry_codes: None,
                max_failover_items: 10,
                no_trying_timeout_ms: None,
                items: vec![short_item, long_item],
            },
        )
        .await;
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Config Trie Sell Deck".to_string(),
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
    crate::wholesale_helpers::load_runtime_routing_profiles(&route_invite.state, &db).await;

    let cookie = source_cookie(Some(inbound.id));

    let (request, option) = make_invite("12125550100", "config_trie_user");
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route invite success");

    let RouteResult::Forward(option, _) = result else {
        panic!("Matching prefixes must resolve to one selected carrier");
    };
    let destination = option.destination.unwrap().addr.to_string();
    assert!(destination == "10.10.0.1:5060" || destination == "10.10.0.2:5060");
}

#[tokio::test]
async fn test_wholesale_route_prefix_stripping_and_cost() {
    let db = setup_db().await;

    // 1. Create Carrier Trunk (Destination)
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier Trunk".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        incoming_to_user_prefix: Set(Some("9999".to_string())), // Add tech prefix to outbound trunk
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound trunk");

    // 1.5 Create Wholesale Trunk Config for Carrier (to set Buy Rate Deck)
    let buy_deck = rate_deck::ActiveModel {
        name: Set("Buy Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck");

    // Buy Rate for "44" (UK)
    rate::ActiveModel {
        deck_id: Set(buy_deck.id),
        prefix: Set("44".to_string()),
        rate: Set(0.00050), // Buy rate
        min_duration: Set(1),
        increment: Set(1),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy rate");

    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier_trunk.id),
        rate_deck_id: Set(Some(buy_deck.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wholesale trunk config");

    // 2. Create Source Trunk (Origin) with Prefix "7231"
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source Trunk".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        incoming_to_user_prefix: Set(Some("7231".to_string())),
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

    // 5. Create Rate Deck & Rate
    let deck = rate_deck::ActiveModel {
        name: Set("Test Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

    // Rate for "44" (UK)
    rate::ActiveModel {
        deck_id: Set(deck.id),
        prefix: Set("44".to_string()),
        rate: Set(0.00076),
        min_duration: Set(1),
        increment: Set(1),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create rate");

    rate::ActiveModel {
        deck_id: Set(deck.id),
        prefix: Set("44".to_string()),
        match_caller_prefix: Set(Some("447".to_string())),
        rate: Set(0.00099),
        min_duration: Set(1),
        increment: Set(1),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create caller-specific rate");

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
    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new()),
    };

    // Persist rates before rebuilding runtime state.
    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Test Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![
                    RateConfig {
                        prefix: "44".to_string(),
                        match_caller_prefix: None,
                        rate: 0.00076,
                        min_duration: 1,
                        increment: 1,
                        remark: None,
                    },
                    RateConfig {
                        prefix: "44".to_string(),
                        match_caller_prefix: Some("447".to_string()),
                        rate: 0.00099,
                        min_duration: 1,
                        increment: 1,
                        remark: None,
                    },
                ],
            },
        )
        .await;
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: buy_deck.id,
                name: "Buy Deck".to_string(),
                description: None,
                r#type: "buy".to_string(),
                rates: vec![RateConfig {
                    prefix: "44".to_string(),
                    match_caller_prefix: None,
                    rate: 0.0005,
                    min_duration: 1,
                    increment: 1,
                    remark: None,
                }],
            },
        )
        .await;
    }
    load_test_routing_profile(
        &route_invite.state,
        &db,
        &profile,
        vec![RoutingProfileItemConfig {
            sip_trunk_id: carrier_trunk.id,
            priority: 0,
            weight: 100,
            ..Default::default()
        }],
    )
    .await;

    // 9. Create INVITE
    // From: 447903193562
    // To: 7231447425907330 (Prefix 7231 + 44...)
    let cookie = source_cookie(Some(inbound.id));

    let invite_uri = rsipstack::sip::Uri::try_from("sip:7231447425907330@192.168.1.1").unwrap();
    let from_uri = rsipstack::sip::Uri::try_from("sip:447903193562@192.168.1.1").unwrap();
    let to_uri = rsipstack::sip::Uri::try_from("sip:7231447425907330@192.168.1.1").unwrap();

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
                "callid_prefix_test",
            )),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bK",
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

    // 10. Run Route
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route invite success");

    // 11. Assert
    match result {
        RouteResult::Forward(opt, _) => {
            // Check if destination is set to outbound trunk
            assert!(opt.destination.is_some());

            // Verify that the rate was found and stored in the cookie
            // The caller-specific sell rate is selected over the default sell rate.
            let billing = cookie
                .get_extension::<WholesaleBillingContext>()
                .expect("billing in cookie");
            assert_eq!(billing.sell_rate, 0.00099);

            // Verify Vendor Rate (Buy Rate)
            assert_eq!(billing.buy_rate, 0.0005);

            // Verify that the callee sent to the outbound has the prefix stripped AND the outbound tech prefix added
            // Original: 723144...
            // Stripped: 44...
            // Carrier Prefix: 9999
            // Result: 999944...
            let new_callee = opt.callee.user().unwrap();
            assert_eq!(new_callee, "9999447425907330");
        }
        RouteResult::Abort(code, msg) => {
            panic!("Route aborted: {:?} {:?}", code, msg);
        }
        _ => panic!("Expected Forward result"),
    }
}

/// Build a simple INVITE request for test purposes.
fn make_invite(callee_user: &str, caller_user: &str) -> (rsipstack::sip::Request, InviteOption) {
    let invite_uri =
        rsipstack::sip::Uri::try_from(format!("sip:{}@192.168.1.1", callee_user)).unwrap();
    let from_uri =
        rsipstack::sip::Uri::try_from(format!("sip:{}@192.168.1.1", caller_user)).unwrap();
    let to_uri = rsipstack::sip::Uri::try_from(format!("sip:{}@192.168.1.1", callee_user)).unwrap();

    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(format!(
                "<{}>;tag=abc",
                from_uri
            ))),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(format!("<{}>", to_uri))),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("test-call-id")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bK99",
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
    (request, option)
}

#[tokio::test]
async fn test_wholesale_route_full_prefix_chain() {
    let db = setup_db().await;

    // 1. Carrier Trunk with Tech Prefix "00"
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier".to_string()),
        sip_server: Set(Some("1.2.3.4:5060".to_string())),
        incoming_to_user_prefix: Set(Some("00".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound trunk");

    let buy_deck = rate_deck::ActiveModel {
        name: Set("Buy Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck");

    // Buy Rate for "88123" (Profile Prepend "88" + Callee "123")
    rate::ActiveModel {
        deck_id: Set(buy_deck.id),
        prefix: Set("88123".to_string()),
        rate: Set(0.0005),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy rate");

    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier_trunk.id),
        rate_deck_id: Set(Some(buy_deck.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create wholesale trunk config");

    // 2. Source Trunk with Prefix "99"
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        incoming_to_user_prefix: Set(Some("99".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    // 3. Routing Profile with Prepend "88"
    let profile = routing_profile::ActiveModel {
        name: Set("Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    // 4. Sell Rate for "88123"
    let sell_deck = rate_deck::ActiveModel {
        name: Set("Sell Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create sell deck");

    rate::ActiveModel {
        deck_id: Set(sell_deck.id),
        prefix: Set("123".to_string()),
        rate: Set(0.001),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create sell rate");

    // 5. Tenant
    let tenant = tenant::ActiveModel {
        name: Set("Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(sell_deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk link");

    // 6. Setup
    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state: Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new()),
    };

    // Persist rates before rebuilding runtime state.
    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: sell_deck.id,
                name: "Sell Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                    prefix: "123".to_string(),
                    match_caller_prefix: None,
                    rate: 0.001,
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
                id: buy_deck.id,
                name: "Buy Deck".to_string(),
                description: None,
                r#type: "buy".to_string(),
                rates: vec![RateConfig {
                    prefix: "123".to_string(),
                    match_caller_prefix: None,
                    rate: 0.0005,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }
    load_test_routing_profile(
        &route_invite.state,
        &db,
        &profile,
        vec![RoutingProfileItemConfig {
            sip_trunk_id: carrier_trunk.id,
            priority: 0,
            weight: 100,
            prepend_digits: Some("88".to_string()),
            ..Default::default()
        }],
    )
    .await;

    // 7. INVITE to 99123456
    let cookie = source_cookie(Some(inbound.id));

    let invite_uri = rsipstack::sip::Uri::try_from("sip:99123456@127.0.0.1").unwrap();
    let request = rsipstack::sip::Request {
        method: rsipstack::sip::Method::Invite,
        uri: invite_uri.clone(),
        headers: vec![
            rsipstack::sip::Header::From(rsipstack::sip::headers::From::new(
                "<sip:test@127.0.0.1>;tag=1",
            )),
            rsipstack::sip::Header::To(rsipstack::sip::headers::To::new(
                "<sip:99123456@127.0.0.1>",
            )),
            rsipstack::sip::Header::CallId(rsipstack::sip::headers::CallId::new("cid")),
            rsipstack::sip::Header::CSeq(rsipstack::sip::headers::CSeq::new("1 INVITE")),
            rsipstack::sip::Header::Via(rsipstack::sip::headers::Via::new(
                "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bK",
            )),
        ]
        .into(),
        version: rsipstack::sip::Version::V2,
        body: Default::default(),
    };

    let result = route_invite
        .route_invite(
            InviteOption {
                callee: invite_uri.clone(),
                caller: rsipstack::sip::Uri::try_from("sip:test@127.0.0.1").unwrap(),
                ..Default::default()
            },
            &request,
            &DialDirection::Outbound,
            &cookie,
        )
        .await
        .expect("route success");

    if let RouteResult::Forward(opt, _) = result {
        assert_eq!(opt.callee.user().unwrap(), "0088123456");
        let billing = cookie.get_extension::<WholesaleBillingContext>().unwrap();
        assert_eq!(billing.sell_rate, 0.001);
        assert_eq!(billing.buy_rate, 0.0005);
    } else {
        panic!("Expected Forward");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// LCR (Least Cost Routing) Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Helper to setup LCR test with multiple outbound trunks at different costs.
/// Returns the tenant, inbound trunk, profile, carriers, and rate decks.
async fn setup_lcr_test(
    db: &DatabaseConnection,
) -> (
    i64,
    i64,
    routing_profile::Model,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
) {
    // Create 3 outbound trunks with different costs
    let carrier1 = sip_trunk::ActiveModel {
        name: Set("Carrier-Cheap".to_string()),
        sip_server: Set(Some("10.0.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create outbound 1");

    let carrier2 = sip_trunk::ActiveModel {
        name: Set("Carrier-Medium".to_string()),
        sip_server: Set(Some("10.0.0.2:5060".to_string())),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create outbound 2");

    let carrier3 = sip_trunk::ActiveModel {
        name: Set("Carrier-Expensive".to_string()),
        sip_server: Set(Some("10.0.0.3:5060".to_string())),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create outbound 3");

    // Source trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("LCR-Source".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create source trunk");

    // Create buy decks for each outbound
    let buy_deck1 = rate_deck::ActiveModel {
        name: Set("Buy-Cheap".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create buy deck 1");

    let buy_deck2 = rate_deck::ActiveModel {
        name: Set("Buy-Medium".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create buy deck 2");

    let buy_deck3 = rate_deck::ActiveModel {
        name: Set("Buy-Expensive".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create buy deck 3");

    // Buy rates: Carrier1 = 0.01, Carrier2 = 0.02, Carrier3 = 0.03
    rate::ActiveModel {
        deck_id: Set(buy_deck1.id),
        prefix: Set("1".to_string()),
        rate: Set(0.01),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create buy rate 1");

    rate::ActiveModel {
        deck_id: Set(buy_deck2.id),
        prefix: Set("1".to_string()),
        rate: Set(0.02),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create buy rate 2");

    rate::ActiveModel {
        deck_id: Set(buy_deck3.id),
        prefix: Set("1".to_string()),
        rate: Set(0.03),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create buy rate 3");

    // Link buy decks to carriers
    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier1.id),
        rate_deck_id: Set(Some(buy_deck1.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create trunk config 1");

    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier2.id),
        rate_deck_id: Set(Some(buy_deck2.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create trunk config 2");

    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier3.id),
        rate_deck_id: Set(Some(buy_deck3.id)),
        ringback: Set(None),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create trunk config 3");

    // Routing profile with all 3 carriers at same priority
    let profile = routing_profile::ActiveModel {
        name: Set("LCR-Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create profile");

    // Sell rate deck
    let sell_deck = rate_deck::ActiveModel {
        name: Set("LCR-Sell".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create sell deck");

    rate::ActiveModel {
        deck_id: Set(sell_deck.id),
        prefix: Set("1".to_string()),
        rate: Set(0.05),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create sell rate");

    // Tenant with LCR enabled
    let tenant = tenant::ActiveModel {
        name: Set("LCR-Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(sell_deck.id)),
        lcr_enabled: Set(true),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create tenant");

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create tenant trunk link");

    (
        tenant.id,
        inbound.id,
        profile,
        carrier1.id,
        carrier2.id,
        carrier3.id,
        buy_deck1.id,
        buy_deck2.id,
        buy_deck3.id,
        sell_deck.id,
    )
}

/// LCR enabled routes same-priority trunks by buy_rate.
#[tokio::test]
async fn test_lcr_sorts_by_buy_rate() {
    let db = setup_db().await;
    let (
        _tenant_id,
        inbound_id,
        profile,
        cheap_carrier_id,
        medium_carrier_id,
        expensive_carrier_id,
        buy1,
        buy2,
        buy3,
        sell,
    ) = setup_lcr_test(&db).await;

    // Create WholesaleRouteInvite
    let state = Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new());

    // Persist rates before rebuilding runtime state.
    {
        // Sell deck
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: sell,
                name: "Sell".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.05,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
        // Buy decks
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: buy1,
                name: "Buy-Cheap".to_string(),
                description: None,
                r#type: "buy".to_string(),
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
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: buy2,
                name: "Buy-Medium".to_string(),
                description: None,
                r#type: "buy".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.02,
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
                id: buy3,
                name: "Buy-Expensive".to_string(),
                description: None,
                r#type: "buy".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.03,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }
    load_test_routing_profile(
        &state,
        &db,
        &profile,
        vec![
            RoutingProfileItemConfig {
                sip_trunk_id: expensive_carrier_id,
                priority: 1,
                weight: 100,
                ..Default::default()
            },
            RoutingProfileItemConfig {
                sip_trunk_id: cheap_carrier_id,
                priority: 1,
                weight: 100,
                ..Default::default()
            },
            RoutingProfileItemConfig {
                sip_trunk_id: medium_carrier_id,
                priority: 1,
                weight: 100,
                ..Default::default()
            },
        ],
    )
    .await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state,
    };

    let cookie = source_cookie(Some(inbound_id));

    let (request, option) = make_invite("12125551234", "lcr_test");
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route should succeed");

    // LCR should select the cheapest same-priority trunk, regardless of DB order.
    let RouteResult::Forward(option, _) = result else {
        panic!("Expected Forward result");
    };
    assert_eq!(
        option.destination.unwrap().addr.to_string(),
        "10.0.0.1:5060",
        "LCR should route to the cheapest outbound first"
    );
}

/// LCR disabled same-priority routes use weight and do not sort by buy_rate.
#[tokio::test]
async fn test_same_priority_uses_weight_when_lcr_disabled() {
    let db = setup_db().await;

    // Similar setup with old lcr_enabled data disabled.
    let carrier1 = sip_trunk::ActiveModel {
        name: Set("Carrier-Cheap".to_string()),
        sip_server: Set(Some("10.1.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound 1");

    let carrier2 = sip_trunk::ActiveModel {
        name: Set("Carrier-Expensive".to_string()),
        sip_server: Set(Some("10.1.0.2:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound 2");

    let inbound = sip_trunk::ActiveModel {
        name: Set("LCR-Disabled-Source".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    // Buy decks
    let buy_deck1 = rate_deck::ActiveModel {
        name: Set("Buy-Cheap2".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck 1");

    let buy_deck2 = rate_deck::ActiveModel {
        name: Set("Buy-Expensive2".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck 2");

    rate::ActiveModel {
        deck_id: Set(buy_deck1.id),
        prefix: Set("1".to_string()),
        rate: Set(0.01),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy rate 1");

    rate::ActiveModel {
        deck_id: Set(buy_deck2.id),
        prefix: Set("1".to_string()),
        rate: Set(0.05),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy rate 2");

    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier1.id),
        rate_deck_id: Set(Some(buy_deck1.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create trunk config 1");

    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier2.id),
        rate_deck_id: Set(Some(buy_deck2.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create trunk config 2");

    // Profile with expensive outbound first
    let profile = routing_profile::ActiveModel {
        name: Set("LCR-Disabled-Profile".to_string()),
        created_at: Set(Utc::now()),
        updated_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    let sell_deck = rate_deck::ActiveModel {
        name: Set("LCR-Disabled-Sell".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create sell deck");

    rate::ActiveModel {
        deck_id: Set(sell_deck.id),
        prefix: Set("1".to_string()),
        rate: Set(0.10),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create sell rate");

    // Tenant with old lcr_enabled data disabled.
    let tenant = tenant::ActiveModel {
        name: Set("LCR-Disabled-Tenant".to_string()),
        balance: Set(100.0),
        routing_profile_id: Set(Some(profile.id)),
        rate_deck_id: Set(Some(sell_deck.id)),
        lcr_enabled: Set(false),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk link");

    // Setup route invite
    let state = Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new());

    // Persist rates before rebuilding runtime state.
    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: sell_deck.id,
                name: "Sell".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.10,
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
                id: buy_deck1.id,
                name: "Buy-Cheap".to_string(),
                description: None,
                r#type: "buy".to_string(),
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
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: buy_deck2.id,
                name: "Buy-Expensive".to_string(),
                description: None,
                r#type: "buy".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.05,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
    }
    load_test_routing_profile(
        &state,
        &db,
        &profile,
        vec![
            RoutingProfileItemConfig {
                sip_trunk_id: carrier2.id,
                priority: 1,
                weight: 100,
                ..Default::default()
            },
            RoutingProfileItemConfig {
                sip_trunk_id: carrier1.id,
                priority: 1,
                weight: 0,
                ..Default::default()
            },
        ],
    )
    .await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state,
    };

    let cookie = source_cookie(Some(inbound.id));

    let (request, option) = make_invite("12125559999", "lcr_disabled_test");
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route should succeed");

    // Same-priority routing uses weight. The expensive outbound has all the weight.
    let RouteResult::Forward(option, _) = result else {
        panic!("Expected Forward result");
    };
    assert_eq!(
        option.destination.unwrap().addr.to_string(),
        "10.1.0.2:5060",
        "Same-priority routing should select by weight"
    );
}

/// LCR still respects priority before buy_rate.
#[tokio::test]
async fn test_priority_over_cost_with_lcr_enabled() {
    let db = setup_db().await;

    // Create 2 outbound trunks with different priorities and costs
    let carrier_high_priority_expensive = sip_trunk::ActiveModel {
        name: Set("Carrier-HighPrio-Expensive".to_string()),
        sip_server: Set(Some("10.2.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound 1");

    let carrier_low_priority_cheap = sip_trunk::ActiveModel {
        name: Set("Carrier-LowPrio-Cheap".to_string()),
        sip_server: Set(Some("10.2.0.2:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound 2");

    // Source trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("LCR-Priority-Source".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    // Buy decks
    let buy_deck_expensive = rate_deck::ActiveModel {
        name: Set("Buy-HighPrio-Expensive".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck 1");

    let buy_deck_cheap = rate_deck::ActiveModel {
        name: Set("Buy-LowPrio-Cheap".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck 2");

    // Expensive rate (0.05)
    rate::ActiveModel {
        deck_id: Set(buy_deck_expensive.id),
        prefix: Set("1".to_string()),
        rate: Set(0.05),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy rate 1");

    // Cheap rate (0.01)
    rate::ActiveModel {
        deck_id: Set(buy_deck_cheap.id),
        prefix: Set("1".to_string()),
        rate: Set(0.01),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy rate 2");

    // Link buy decks to carriers
    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier_high_priority_expensive.id),
        rate_deck_id: Set(Some(buy_deck_expensive.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create trunk config 1");

    rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier_low_priority_cheap.id),
        rate_deck_id: Set(Some(buy_deck_cheap.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create trunk config 2");

    // Sell deck
    let sell_deck = rate_deck::ActiveModel {
        name: Set("Sell-Priority-Test".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create sell deck");

    rate::ActiveModel {
        deck_id: Set(sell_deck.id),
        prefix: Set("1".to_string()),
        rate: Set(0.10),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create sell rate");

    // Routing profile with two items at different priorities
    let profile = routing_profile::ActiveModel {
        name: Set("LCR-Priority-Profile".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create routing profile");

    // Tenant with LCR enabled
    let tenant = tenant::ActiveModel {
        name: Set("LCR-Priority-Tenant".to_string()),
        balance: Set(100.0),
        rate_deck_id: Set(Some(sell_deck.id)),
        routing_profile_id: Set(Some(profile.id)),
        lcr_enabled: Set(true),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    // Link source trunk to tenant
    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk");

    // Setup route invite
    let state = Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new());

    // Persist rates before rebuilding runtime state.
    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: sell_deck.id,
                name: "Sell-Priority-Test".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.10,
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
                id: buy_deck_expensive.id,
                name: "Buy-HighPrio-Expensive".to_string(),
                description: None,
                r#type: "buy".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.05,
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
                id: buy_deck_cheap.id,
                name: "Buy-LowPrio-Cheap".to_string(),
                description: None,
                r#type: "buy".to_string(),
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
    load_test_routing_profile(
        &state,
        &db,
        &profile,
        vec![
            RoutingProfileItemConfig {
                sip_trunk_id: carrier_high_priority_expensive.id,
                priority: 1,
                ..Default::default()
            },
            RoutingProfileItemConfig {
                sip_trunk_id: carrier_low_priority_cheap.id,
                priority: 10,
                ..Default::default()
            },
        ],
    )
    .await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state,
    };

    let cookie = source_cookie(Some(inbound.id));

    let (request, option) = make_invite("12125557777", "lcr_priority_test");
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route should succeed");

    // Priority should still decide before cost.
    match result {
        RouteResult::Forward(opt, _) => {
            let dest = opt.destination.unwrap().addr.to_string();
            assert_eq!(dest, "10.2.0.1:5060", "Priority should win over buy rate");
        }
        _ => panic!("Expected Forward result"),
    }
}

/// LCR same-priority trunks should sort by buy_rate.
#[tokio::test]
async fn test_lcr_same_priority_sorted_by_cost() {
    let db = setup_db().await;

    // Create 3 outbound trunks with SAME priority but different costs
    let carrier_expensive = sip_trunk::ActiveModel {
        name: Set("Carrier-SamePrio-Expensive".to_string()),
        sip_server: Set(Some("10.3.0.3:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound 1");

    let carrier_medium = sip_trunk::ActiveModel {
        name: Set("Carrier-SamePrio-Medium".to_string()),
        sip_server: Set(Some("10.3.0.2:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound 2");

    let carrier_cheap = sip_trunk::ActiveModel {
        name: Set("Carrier-SamePrio-Cheap".to_string()),
        sip_server: Set(Some("10.3.0.1:5060".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound 3");

    // Source trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("LCR-SamePrio-Source".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    // Buy decks with different rates
    let buy_deck_expensive = rate_deck::ActiveModel {
        name: Set("Buy-SamePrio-Expensive".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck 1");

    let buy_deck_medium = rate_deck::ActiveModel {
        name: Set("Buy-SamePrio-Medium".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck 2");

    let buy_deck_cheap = rate_deck::ActiveModel {
        name: Set("Buy-SamePrio-Cheap".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create buy deck 3");

    // Rates: 0.05, 0.03, 0.01
    for (deck_id, rate_val) in [
        (buy_deck_expensive.id, 0.05),
        (buy_deck_medium.id, 0.03),
        (buy_deck_cheap.id, 0.01),
    ] {
        rate::ActiveModel {
            deck_id: Set(deck_id),
            prefix: Set("1".to_string()),
            rate: Set(rate_val),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create buy rate");
    }

    // Link buy decks to carriers
    for (carrier_id, deck_id) in [
        (carrier_expensive.id, buy_deck_expensive.id),
        (carrier_medium.id, buy_deck_medium.id),
        (carrier_cheap.id, buy_deck_cheap.id),
    ] {
        rustpbx::addons::wholesale::models::wholesale_trunk_config::ActiveModel {
            sip_trunk_id: Set(carrier_id),
            rate_deck_id: Set(Some(deck_id)),
            ringback: Set(None),

            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create trunk config");
    }

    // Sell deck
    let sell_deck = rate_deck::ActiveModel {
        name: Set("Sell-SamePrio-Test".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create sell deck");

    rate::ActiveModel {
        deck_id: Set(sell_deck.id),
        prefix: Set("1".to_string()),
        rate: Set(0.10),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create sell rate");

    // Routing profile with all items at SAME priority (5)
    let profile = routing_profile::ActiveModel {
        name: Set("LCR-SamePrio-Profile".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create routing profile");

    // Tenant with LCR enabled
    let tenant = tenant::ActiveModel {
        name: Set("LCR-SamePrio-Tenant".to_string()),
        balance: Set(100.0),
        rate_deck_id: Set(Some(sell_deck.id)),
        routing_profile_id: Set(Some(profile.id)),
        lcr_enabled: Set(true),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    // Link source trunk to tenant
    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk");

    // Setup route invite
    let state = Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new());

    // Persist rates before rebuilding runtime state.
    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: sell_deck.id,
                name: "Sell-SamePrio-Test".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![RateConfig {
                    prefix: "1".to_string(),
                    match_caller_prefix: None,
                    rate: 0.10,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
        for (deck_id, name, rate_val) in [
            (buy_deck_expensive.id, "Buy-SamePrio-Expensive", 0.05),
            (buy_deck_medium.id, "Buy-SamePrio-Medium", 0.03),
            (buy_deck_cheap.id, "Buy-SamePrio-Cheap", 0.01),
        ] {
            crate::wholesale_helpers::insert_runtime_rate_deck(
                &db,
                RateDeckConfig {
                    id: deck_id,
                    name: name.to_string(),
                    description: None,
                    r#type: "buy".to_string(),
                    rates: vec![RateConfig {
                        prefix: "1".to_string(),
                        match_caller_prefix: None,
                        rate: rate_val,
                        min_duration: 60,
                        increment: 60,
                        remark: None,
                    }],
                },
            )
            .await;
        }
    }
    load_test_routing_profile(
        &state,
        &db,
        &profile,
        vec![
            RoutingProfileItemConfig {
                sip_trunk_id: carrier_expensive.id,
                priority: 5,
                ..Default::default()
            },
            RoutingProfileItemConfig {
                sip_trunk_id: carrier_medium.id,
                priority: 5,
                ..Default::default()
            },
            RoutingProfileItemConfig {
                sip_trunk_id: carrier_cheap.id,
                priority: 5,
                ..Default::default()
            },
        ],
    )
    .await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state,
    };

    let cookie = source_cookie(Some(inbound.id));

    let (request, option) = make_invite("12125556666", "lcr_same_prio_test");
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route should succeed");

    // With same priority, LCR should sort by cost; cheapest should be first.
    let RouteResult::Forward(option, _) = result else {
        panic!("Expected Forward result");
    };
    assert_eq!(
        option.destination.unwrap().addr.to_string(),
        "10.3.0.1:5060",
        "LCR should route to cheapest outbound when priorities are equal"
    );
}

/// Test that rewrite_hostport=true rewrites callee host to trunk's sip_server
#[tokio::test]
async fn test_wholesale_route_rewrite_hostport_true() {
    let db = setup_db().await;

    // Create Carrier Trunk with rewrite_hostport = true (default)
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier-Rewrite-True".to_string()),
        sip_server: Set(Some("outbound.example.com:5080".to_string())),
        rewrite_hostport: Set(true),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound trunk");

    // Create Source Trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source-Rewrite-Test".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    // Create Rate Deck & Rate (required for routing)
    let deck = rate_deck::ActiveModel {
        name: Set("Rewrite-Test-Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

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
    .expect("create rate");

    // Create Tenant with rate deck
    let tenant = tenant::ActiveModel {
        name: Set("Tenant-Rewrite-Test".to_string()),
        balance: Set(100.0),
        rate_deck_id: Set(Some(deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    // Link source trunk to tenant
    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk");

    // Create Routing Profile
    let profile = routing_profile::ActiveModel {
        name: Set("Rewrite-Test-Profile".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    // Assign profile to tenant
    tenant::ActiveModel {
        id: Set(tenant.id),
        routing_profile_id: Set(Some(profile.id)),
        ..Default::default()
    }
    .save(&db)
    .await
    .expect("update tenant profile");

    // Setup route invite
    let state = Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new());

    // Persist rates before rebuilding runtime state.
    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "Rewrite-Test-Deck".to_string(),
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
    load_test_routing_profile(
        &state,
        &db,
        &profile,
        vec![RoutingProfileItemConfig {
            sip_trunk_id: carrier_trunk.id,
            priority: 0,
            weight: 100,
            ..Default::default()
        }],
    )
    .await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state,
    };

    let cookie = source_cookie(Some(inbound.id));

    let (request, option) = make_invite("12125556666", "test_caller");
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route should succeed");

    let RouteResult::Forward(option, _) = result else {
        panic!("Expected Forward result");
    };
    // When rewrite_hostport=true, callee host should be rewritten to trunk's sip_server
    assert_eq!(
        option.callee.host().to_string(),
        "outbound.example.com",
        "callee host should be rewritten to trunk sip_server when rewrite_hostport=true"
    );
}

/// Test that wholesale routing ignores rewrite_hostport=false and still uses outbound host.
#[tokio::test]
async fn test_wholesale_route_ignores_rewrite_hostport_false() {
    let db = setup_db().await;

    // Create Carrier Trunk with rewrite_hostport = false. Wholesale ignores this flag.
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier-Rewrite-False".to_string()),
        sip_server: Set(Some("outbound.example.com:5080".to_string())),
        rewrite_hostport: Set(false),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create outbound trunk");

    // Create Source Trunk
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source-NoRewrite-Test".to_string()),
        allowed_ips: Set(Some(test_source_allowed_ips())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create source trunk");

    // Create Rate Deck & Rate (required for routing)
    let deck = rate_deck::ActiveModel {
        name: Set("NoRewrite-Test-Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create deck");

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
    .expect("create rate");

    // Create Tenant with rate deck
    let tenant = tenant::ActiveModel {
        name: Set("Tenant-NoRewrite-Test".to_string()),
        balance: Set(100.0),
        rate_deck_id: Set(Some(deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant");

    // Link source trunk to tenant
    tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create tenant trunk");

    // Create Routing Profile
    let profile = routing_profile::ActiveModel {
        name: Set("NoRewrite-Test-Profile".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("create profile");

    // Assign profile to tenant
    tenant::ActiveModel {
        id: Set(tenant.id),
        routing_profile_id: Set(Some(profile.id)),
        ..Default::default()
    }
    .save(&db)
    .await
    .expect("update tenant profile");

    // Setup route invite
    let state = Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new());

    // Persist rates before rebuilding runtime state.
    {
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            RateDeckConfig {
                id: deck.id,
                name: "NoRewrite-Test-Deck".to_string(),
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
    load_test_routing_profile(
        &state,
        &db,
        &profile,
        vec![RoutingProfileItemConfig {
            sip_trunk_id: carrier_trunk.id,
            priority: 0,
            weight: 100,
            ..Default::default()
        }],
    )
    .await;

    let route_invite = WholesaleRouteInvite {
        db: db.clone(),
        state,
    };

    let cookie = source_cookie(Some(inbound.id));

    // Create invite with original callee host (192.168.1.1 from make_invite).
    let (request, option) = make_invite("12125556666", "test_caller");
    let result = route_invite
        .route_invite(option, &request, &DialDirection::Outbound, &cookie)
        .await
        .expect("route should succeed");

    let RouteResult::Forward(option, _) = result else {
        panic!("Expected Forward result");
    };
    assert_eq!(
        option.callee.host().to_string(),
        "outbound.example.com",
        "wholesale should use outbound host even when rewrite_hostport=false"
    );

    let destination = option.destination.unwrap();
    assert_eq!(
        destination.addr.host.to_string(),
        "outbound.example.com",
        "destination should still be set to trunk sip_server"
    );
}
