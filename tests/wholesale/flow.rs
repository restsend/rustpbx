use chrono::{Duration, Utc};
use rustpbx::addons::Addon;
use rustpbx::addons::wholesale::{
    WholesaleAddon,
    data::{RateConfig, RateMatcher},
    migration::Migrator as WholesaleMigrator,
    models::{rate, rate_deck, tenant, wholesale_cdr, wholesale_trunk_config},
    route::WholesaleBillingContext,
};
use rustpbx::callrecord::{CallDetails, CallRecord};
use rustpbx::models::{
    call_record::extract_sip_username, migration::Migrator as MainMigrator, sip_trunk,
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Database, DatabaseConnection, EntityTrait,
    QueryFilter,
};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use std::collections::HashMap;

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

async fn create_fixtures(db: &DatabaseConnection) -> (i64, i64, i64, i64) {
    // 1. Create Carrier & Trunk
    let trunk = sip_trunk::ActiveModel {
        name: Set("Carrier Trunk".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create sip trunk");

    // 2. Create Rate Decks
    let sell_deck = rate_deck::ActiveModel {
        name: Set("Sell Deck".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create sell deck");

    let buy_deck = rate_deck::ActiveModel {
        name: Set("Buy Deck".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create buy deck");

    // 3. Create Rates
    // Sell Rate: Prefix 1, Rate 0.1, Min 60, Inc 60
    rate::ActiveModel {
        deck_id: Set(sell_deck.id),
        prefix: Set("1".to_string()),
        rate: Set(0.1),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create sell rate");

    // Buy Rate: Prefix 1, Rate 0.05, Min 60, Inc 60
    rate::ActiveModel {
        deck_id: Set(buy_deck.id),
        prefix: Set("1".to_string()),
        rate: Set(0.05),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create buy rate");

    // 4. Create Tenant
    let tenant = tenant::ActiveModel {
        name: Set("Test Tenant".to_string()),
        balance: Set(100.0), // Initial balance
        rate_deck_id: Set(Some(sell_deck.id)),
        // status field removed
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create tenant");

    // 5. Configure Trunk Buy Rate
    wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(trunk.id),
        rate_deck_id: Set(Some(buy_deck.id)),
        ringback: Set(None),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("create trunk config");

    (tenant.id, trunk.id, sell_deck.id, buy_deck.id)
}

#[tokio::test]
async fn test_wholesale_billing_flow() {
    let db = setup_db().await;
    let (tenant_id, trunk_id, sell_deck_id, buy_deck_id) = create_fixtures(&db).await;

    let call_id = "test-call-id-123";
    let mut extras = HashMap::new();
    extras.insert(
        "wholesale_tenant_id".to_string(),
        json!(tenant_id.to_string()),
    );
    extras.insert("wholesale_trunk_id".to_string(), json!(trunk_id));

    // Simulate Route Logic: Find Rates
    // 1. Tenant Rate (Sell)
    let tenant_rate_record = rate::Entity::find()
        .filter(rate::Column::DeckId.eq(sell_deck_id))
        .filter(rate::Column::Prefix.eq("1")) // Simplified matching
        .one(&db)
        .await
        .expect("find sell rate")
        .expect("sell rate exists");

    // 2. Vendor Rate (Buy)
    let vendor_rate_record = rate::Entity::find()
        .filter(rate::Column::DeckId.eq(buy_deck_id))
        .filter(rate::Column::Prefix.eq("1"))
        .one(&db)
        .await
        .expect("find buy rate")
        .expect("buy rate exists");

    extras.insert(
        "wholesale_tenant_rate".to_string(),
        json!(tenant_rate_record.rate.to_string()),
    );
    extras.insert(
        "wholesale_tenant_min_duration".to_string(),
        json!(tenant_rate_record.min_duration.to_string()),
    );
    extras.insert(
        "wholesale_tenant_increment".to_string(),
        json!(tenant_rate_record.increment.to_string()),
    );

    extras.insert(
        "wholesale_vendor_rate".to_string(),
        json!(vendor_rate_record.rate.to_string()),
    );
    extras.insert(
        "wholesale_vendor_min_duration".to_string(),
        json!(vendor_rate_record.min_duration.to_string()),
    );
    extras.insert(
        "wholesale_vendor_increment".to_string(),
        json!(vendor_rate_record.increment.to_string()),
    );

    let mut record = CallRecord {
        call_id: call_id.to_string(),
        start_time: Utc::now() - Duration::seconds(120),
        answer_time: Some(Utc::now() - Duration::seconds(120)),
        end_time: Utc::now(),
        caller: "1001".to_string(),
        callee: "1002".to_string(),
        status_code: 200,
        details: CallDetails {
            direction: "inbound".to_string(),
            status: "answered".to_string(),
            from_number: Some("1001".to_string()),
            to_number: Some("1002".to_string()),
            sip_trunk_id: Some(trunk_id),
            ..Default::default()
        },
        ..Default::default()
    };

    record.extensions.insert(WholesaleBillingContext {
        tenant_id,
        carrier_id: Some(trunk_id),
        route_table_id: None,
        route_item_id: None,
        sell_rate: tenant_rate_record.rate,
        buy_rate: vendor_rate_record.rate,
        tenant_min_duration: tenant_rate_record.min_duration,
        tenant_increment: tenant_rate_record.increment,
        vendor_min_duration: vendor_rate_record.min_duration,
        vendor_increment: vendor_rate_record.increment,
        tenant_rate_deck_id: Some(sell_deck_id),
        vendor_rate_deck_id: Some(buy_deck_id),
        reject_code: None,
    });

    let addon = WholesaleAddon::new();
    let hook = addon.call_record_hook(&db).expect("hook exists");

    hook.on_record_completed(std::slice::from_mut(&mut record))
        .await
        .expect("hook execution success");

    // Verify Wholesale CDR
    let w_cdr = wholesale_cdr::Entity::find()
        .filter(wholesale_cdr::Column::CallId.eq(call_id))
        .one(&db)
        .await
        .expect("query w_cdr")
        .expect("w_cdr exists");

    assert_eq!(w_cdr.tenant_id, tenant_id);
    // assert_eq!(w_cdr.carrier_id, Some(trunk_id)); // Check if carrier_id is set correctly
    assert_eq!(w_cdr.duration, 120);

    // Expected calculation:
    // Duration: 120s
    // Sell Rate: 0.1 per min. 120s = 2 mins. Cost = 0.2.
    // Buy Rate: 0.05 per min. 120s = 2 mins. Cost = 0.1.
    // Profit: 0.1.

    assert!(
        (w_cdr.price_total - 0.2).abs() < 1e-6,
        "Price total should be 0.2, got {}",
        w_cdr.price_total
    );
    assert!(
        (w_cdr.cost_total - 0.1).abs() < 1e-6,
        "Cost total should be 0.1, got {}",
        w_cdr.cost_total
    );
    assert!(
        (w_cdr.profit - 0.1).abs() < 1e-6,
        "Profit should be 0.1, got {}",
        w_cdr.profit
    );

    // Verify Tenant Balance Deduction
    let updated_tenant = tenant::Entity::find_by_id(tenant_id)
        .one(&db)
        .await
        .expect("query tenant")
        .expect("tenant exists");

    // Initial balance 100.0 - 0.2 = 99.8
    assert!(
        (updated_tenant.balance - 99.8).abs() < 1e-6,
        "Balance should be 99.8, got {}",
        updated_tenant.balance
    );
}

#[tokio::test]
async fn partial_billing_context_creates_failed_wholesale_cdr() {
    let db = setup_db().await;
    let (tenant_id, _, _, _) = create_fixtures(&db).await;

    let call_id = "tenant-context-rejected-call";
    let start_time = Utc::now() - Duration::seconds(1);
    let mut record = CallRecord {
        call_id: call_id.to_string(),
        start_time,
        end_time: Utc::now(),
        caller: "1001".to_string(),
        callee: "123456".to_string(),
        status_code: 503,
        details: CallDetails {
            direction: "inbound".to_string(),
            status: "failed".to_string(),
            from_number: Some("1001".to_string()),
            to_number: Some("123456".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    record.extensions.insert(WholesaleBillingContext {
        tenant_id,
        ..Default::default()
    });

    let addon = WholesaleAddon::new();
    let hook = addon.call_record_hook(&db).expect("hook exists");
    hook.on_record_completed(std::slice::from_mut(&mut record))
        .await
        .expect("hook execution success");

    let w_cdr = wholesale_cdr::Entity::find()
        .filter(wholesale_cdr::Column::CallId.eq(call_id))
        .one(&db)
        .await
        .expect("query w_cdr")
        .expect("w_cdr exists");

    assert_eq!(w_cdr.tenant_id, tenant_id);
    assert_eq!(w_cdr.carrier_id, None);
    assert_eq!(w_cdr.status, "failed");
    assert_eq!(w_cdr.status_code, Some(503));
    assert_eq!(w_cdr.duration, 0);
    assert_eq!(w_cdr.price_total, 0.0);
    assert_eq!(w_cdr.cost_total, 0.0);
}

#[tokio::test]
async fn enrich_hook_injects_wholesale_reject_code() {
    let db = setup_db().await;
    let (tenant_id, _, _, _) = create_fixtures(&db).await;

    let mut record = CallRecord {
        call_id: "reject-code-enrich".to_string(),
        start_time: Utc::now(),
        end_time: Utc::now(),
        caller: "1001".to_string(),
        callee: "123456".to_string(),
        status_code: 402,
        details: CallDetails {
            direction: "inbound".to_string(),
            status: "failed".to_string(),
            last_error: Some(rustpbx::callrecord::CallRecordLastError {
                code: 402,
                reason: Some("Insufficient funds".to_string()),
            }),
            metadata: Some(std::collections::HashMap::from([
                (
                    "error_code".to_string(),
                    serde_json::Value::String("proxy.route_aborted".to_string()),
                ),
                (
                    "error_app".to_string(),
                    serde_json::Value::String("proxy".to_string()),
                ),
                (
                    "error_severity".to_string(),
                    serde_json::Value::String("warn".to_string()),
                ),
                (
                    "error_message".to_string(),
                    serde_json::Value::String("Route aborted during preview".to_string()),
                ),
            ])),
            ..Default::default()
        },
        ..Default::default()
    };
    record.extensions.insert(WholesaleBillingContext {
        tenant_id,
        reject_code: Some(&rustpbx::addons::wholesale::error_catalog::INSUFFICIENT_FUNDS),
        ..Default::default()
    });

    let addon = WholesaleAddon::new();
    let hook = addon.call_record_hook(&db).expect("hook exists");
    hook.on_record_enrich(std::slice::from_mut(&mut record))
        .await
        .expect("enrich success");

    let meta = record
        .details
        .metadata
        .as_ref()
        .expect("metadata populated by enrich hook");
    assert_eq!(
        meta.get("error_code").and_then(|v| v.as_str()).unwrap(),
        "wholesale.insufficient_funds"
    );
    assert_eq!(
        meta.get("error_app").and_then(|v| v.as_str()).unwrap(),
        "wholesale"
    );
    assert_eq!(
        meta.get("error_severity").and_then(|v| v.as_str()).unwrap(),
        "error"
    );
    assert_eq!(
        meta.get("error_message").and_then(|v| v.as_str()).unwrap(),
        "Insufficient funds"
    );
    // The reject outcome is also recorded as a trace event.
    let trace = meta
        .get("trace")
        .and_then(|v| v.as_array())
        .expect("trace array present");
    assert!(trace.iter().any(|ev| ev["kind"] == "end"));
}

#[tokio::test]
async fn test_rebill_logic_with_prefixes() {
    let db = setup_db().await;

    // 1. Setup Rate Decks
    let sell_deck = rate_deck::ActiveModel {
        name: Set("Sell Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    let buy_deck = rate_deck::ActiveModel {
        name: Set("Buy Deck".to_string()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // 2. Setup Rates (Prefix 86, Rate 0.45)
    rate::ActiveModel {
        deck_id: Set(sell_deck.id),
        prefix: Set("86".to_string()),
        rate: Set(0.45),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    rate::ActiveModel {
        deck_id: Set(buy_deck.id),
        prefix: Set("86".to_string()),
        rate: Set(0.20),
        min_duration: Set(60),
        increment: Set(60),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // 3. Setup Trunks
    // Source Trunk with prefix 99
    let inbound = sip_trunk::ActiveModel {
        name: Set("Source Trunk".to_string()),
        incoming_to_user_prefix: Set(Some("99".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Carrier Trunk with prefix 9413
    let carrier_trunk = sip_trunk::ActiveModel {
        name: Set("Carrier Trunk".to_string()),
        incoming_to_user_prefix: Set(Some("9413".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    wholesale_trunk_config::ActiveModel {
        sip_trunk_id: Set(carrier_trunk.id),
        rate_deck_id: Set(Some(buy_deck.id)),
        ringback: Set(None),

        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // 4. Setup Tenant
    let tenant = tenant::ActiveModel {
        name: Set("Test Tenant".to_string()),
        balance: Set(100.0),
        rate_deck_id: Set(Some(sell_deck.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    rustpbx::addons::wholesale::models::tenant_trunk::ActiveModel {
        tenant_id: Set(tenant.id),
        sip_trunk_id: Set(inbound.id),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // 5. Create Call Record
    let call_id = "test-call-rebill".to_string();
    let start_time = Utc::now() - Duration::seconds(120);
    let _answer_time = start_time + Duration::seconds(10);
    let end_time = Utc::now();

    rustpbx::models::call_record::ActiveModel {
        call_id: Set(call_id.clone()),
        status: Set("answered".to_string()),
        direction: Set("outbound".to_string()),
        started_at: Set(start_time),
        ended_at: Set(Some(end_time)),
        duration_secs: Set(110),
        to_number: Set(Some("94138613800000000".to_string())), // Final destination
        sip_trunk_id: Set(Some(carrier_trunk.id)),             // Destination trunk
        rewrite_original_from: Set(None),
        rewrite_original_to: Set(Some("998613800000000".to_string())),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // 6. Create Wholesale CDR (with wrong initial rates to test rebill)
    wholesale_cdr::ActiveModel {
        call_id: Set(call_id.clone()),
        tenant_id: Set(tenant.id),
        carrier_id: Set(Some(carrier_trunk.id)),
        tenant_rate: Set(0.0), // Wrong
        vendor_rate: Set(0.0), // Wrong
        price_total: Set(0.0),
        cost_total: Set(0.0),
        profit: Set(0.0),
        duration: Set(0),
        status: Set("answered".to_string()),
        status_code: Set(Some(200)),
        caller: Set("100".to_string()),
        callee: Set("94138613800000000".to_string()),
        created_at: Set(Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // 7. Simulate Rebill Logic (Manual implementation of the loop body)
    let cdr = wholesale_cdr::Entity::find()
        .filter(wholesale_cdr::Column::CallId.eq(call_id.clone()))
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let call_record = rustpbx::models::call_record::Entity::find()
        .filter(rustpbx::models::call_record::Column::CallId.eq(call_id.clone()))
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let tenant = tenant::Entity::find_by_id(cdr.tenant_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let original_callee = call_record
        .rewrite_original_to
        .as_ref()
        .map(|s| extract_sip_username(s).unwrap_or_else(|| s.to_string()))
        .unwrap_or_else(|| call_record.to_number.clone().unwrap());

    // Find source trunk
    let links = rustpbx::addons::wholesale::models::tenant_trunk::Entity::find()
        .filter(rustpbx::addons::wholesale::models::tenant_trunk::Column::TenantId.eq(tenant.id))
        .all(&db)
        .await
        .unwrap();

    let mut inbound = None;
    for link in links {
        let t = sip_trunk::Entity::find_by_id(link.sip_trunk_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        if let Some(prefix) = &t.incoming_to_user_prefix {
            if !prefix.is_empty() && original_callee.starts_with(prefix) {
                inbound = Some(t);
                break;
            }
        }
        if inbound.is_none() {
            inbound = Some(t);
        }
    }

    let rated_callee_for_sell = rustpbx::addons::wholesale::bill::normalize_wholesale_number(
        &original_callee,
        inbound.as_ref(),
        None,
    );

    assert_eq!(rated_callee_for_sell, "8613800000000");

    let sell_rates = RateMatcher::from(vec![RateConfig {
        id: 0,
        prefix: "86138".to_string(),
        match_caller_prefix: None,
        rate: 0.45,
        min_duration: 60,
        increment: 60,
        remark: None,
    }]);
    let best_rate = sell_rates
        .find_best_rate(&rated_callee_for_sell, None)
        .unwrap();

    assert_eq!(best_rate.rate, 0.45);
}
