    use rustpbx::addons::wholesale::{
        migration::Migrator as WholesaleMigrator,
        models::{rate, rate_deck, routing_profile, routing_profile_item, tenant, tenant_trunk},
        route::WholesaleRouteInvite,
    };
    use rustpbx::call::{DialDirection, RouteInvite, TransactionCookie, TrunkContext};
    use rustpbx::config::RouteResult;
    use rustpbx::models::{migration::Migrator as MainMigrator, sip_trunk};
    use chrono::Utc;
    use rsipstack::dialog::invitation::InviteOption;
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
    async fn test_caller_pool_selects_from_route_pool() {
        let db = setup_db().await;

        // 1. Create Carrier Trunk
        let carrier_trunk = sip_trunk::ActiveModel {
            name: Set("Carrier Trunk".to_string()),
            sip_server: Set(Some("1.2.3.4:5060".to_string())),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // 2. Create Rate Deck and Rate (required for routing)
        let deck = rate_deck::ActiveModel {
            name: Set("Test Deck".to_string()),
            r#type: Set(rate_deck::RateDeckType::Sell),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        rate::ActiveModel {
            deck_id: Set(deck.id),
            prefix: Set("86".to_string()),
            rate: Set(0.1),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // 3. Create Routing Profile with Caller Pool
        let profile = routing_profile::ActiveModel {
            name: Set("Pool Profile".to_string()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let _pool_item = routing_profile_item::ActiveModel {
            profile_id: Set(profile.id),
            sip_trunk_id: Set(carrier_trunk.id),
            priority: Set(0),
            weight: Set(100),
            caller_selection_policy: Set(Some("pool".to_string())),
            caller_number_pool: Set(Some("1001\n1002\n1003".to_string())),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // 4. Create Tenant
        let tenant = tenant::ActiveModel {
            name: Set("Test Tenant".to_string()),
            routing_profile_id: Set(Some(profile.id)),
            rate_deck_id: Set(Some(deck.id)),
            balance: Set(100.0),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // 5. Create Source Trunk
        let inbound = sip_trunk::ActiveModel {
            name: Set("Source Trunk".to_string()),
            allowed_ips: Set(Some(serde_json::json!(["127.0.0.1"]))),
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

        // 6. Setup WholesaleRouteInvite
        let state = Arc::new(rustpbx::addons::wholesale::data::WholesaleState::new());

        // Persist the rate deck before rebuilding runtime state.
        crate::wholesale_helpers::insert_runtime_rate_deck(
            &db,
            rustpbx::addons::wholesale::data::RateDeckConfig {
                id: deck.id,
                name: "Test Deck".to_string(),
                description: None,
                r#type: "sell".to_string(),
                rates: vec![rustpbx::addons::wholesale::data::RateConfig {
                    prefix: "86".to_string(),
                    match_caller_prefix: None,
                    rate: 0.1,
                    min_duration: 60,
                    increment: 60,
                    remark: None,
                }],
            },
        )
        .await;
        crate::wholesale_helpers::load_runtime_routing_profiles(&state, &db).await;

        let router = WholesaleRouteInvite {
            db: db.clone(),
            state,
        };

        // 7. Test caller selection from route pool
        let request = rsipstack::sip::Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:8613800000000@localhost".try_into().unwrap(),
            headers: vec![
                rsipstack::sip::Header::From("sip:original@localhost".try_into().unwrap()).into(),
                rsipstack::sip::Header::To("sip:8613800000000@localhost".try_into().unwrap())
                    .into(),
                rsipstack::sip::Header::Via(
                    "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK1"
                        .try_into()
                        .unwrap(),
                )
                .into(),
            ]
            .into(),
            version: rsipstack::sip::Version::V2,
            body: Default::default(),
        };

        let cookie = TransactionCookie::default();
        cookie.insert_extension(TrunkContext {
            id: Some(inbound.id),
            name: inbound.name.clone(),
            did_numbers: vec![],
        });

        let option = InviteOption {
            callee: "sip:8613800000000@localhost".try_into().unwrap(),
            caller: "sip:original@localhost".try_into().unwrap(),
            ..Default::default()
        };
        let expected_callers = [
            "1001", "1002", "1003", "1001", "1002", "1003", "1001", "1002",
        ];
        for expected_caller in expected_callers {
            let res = router
                .route_invite(option.clone(), &request, &DialDirection::Inbound, &cookie)
                .await
                .unwrap();
            if let RouteResult::Forward(opt, _) = res {
                let caller = opt.caller.user().unwrap();
                assert_eq!(caller, expected_caller);
            } else {
                panic!("Expected RouteResult::Forward");
            }
        }
    }
