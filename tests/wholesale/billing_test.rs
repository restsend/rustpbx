use rustpbx::addons::wholesale::billing_service::BillingService;
use rustpbx::addons::wholesale::migration::Migrator as WholesaleMigrator;
use rustpbx::addons::wholesale::models::{bill, bill_setting, export_task, tenant, wholesale_cdr};
use rustpbx::models::migration::Migrator as MainMigrator;
use chrono::{Datelike, Duration, TimeZone, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Database, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use sea_orm_migration::MigratorTrait;
use tempfile::TempDir;

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

/// Each test gets its own in-memory DB + isolated temp dir for CSV archives.
/// This prevents parallel tests from racing over the same `storage/bills/` paths.
fn make_service(db: DatabaseConnection, tmp: &TempDir) -> BillingService {
    BillingService::new_with_dir(db, tmp.path())
}

/// Create a tenant with a given name.
async fn create_tenant(db: &DatabaseConnection, name: &str) -> tenant::Model {
    tenant::ActiveModel {
        name: Set(name.to_string()),
        currency: Set("USD".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap_or_else(|e| panic!("create_tenant '{}': {}", name, e))
}

/// Insert a single CDR for the given tenant at `created_at`.
async fn insert_cdr(
    db: &DatabaseConnection,
    tenant_id: i64,
    call_id: &str,
    price: f64,
    duration: i32,
    answered: bool,
    created_at: chrono::DateTime<Utc>,
) -> wholesale_cdr::Model {
    wholesale_cdr::ActiveModel {
        tenant_id: Set(tenant_id),
        call_id: Set(call_id.to_string()),
        price_total: Set(price),
        duration: Set(duration),
        status: Set("completed".to_string()),
        status_code: Set(Some(200)),
        caller: Set("100".to_string()),
        callee: Set("200".to_string()),
        created_at: Set(created_at),
        answer_time: Set(if answered {
            Some(created_at + Duration::seconds(10))
        } else {
            None
        }),
        ring_time: Set(if answered {
            Some(created_at + Duration::seconds(5))
        } else {
            None
        }),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap_or_else(|e| panic!("insert_cdr '{}': {}", call_id, e))
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Basic lifecycle
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_billing_lifecycle() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Billing Test Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    let cdr1 = insert_cdr(
        &db,
        tenant.id,
        "call-1",
        10.0,
        60,
        true,
        now - Duration::hours(2),
    )
    .await;
    let cdr2 = insert_cdr(
        &db,
        tenant.id,
        "call-2",
        20.0,
        120,
        true,
        now - Duration::hours(1),
    )
    .await;

    // unbilled summary = 30
    let unbilled = service.get_unbilled_summary(tenant.id).await.unwrap();
    assert_eq!(unbilled, 30.0);

    let start = now - Duration::days(1);
    let end = now + Duration::days(1);
    let bill = service
        .generate_bill_for_tenant(tenant.id, start, end, true)
        .await
        .expect("Failed to generate bill");

    assert_eq!(bill.total_amount, 30.0);
    assert_eq!(bill.call_count, 2);
    assert_eq!(bill.total_duration, 180);
    assert_eq!(bill.status, "Draft");
    // file_path is None until the async export worker processes the task
    assert!(bill.file_path.is_none());

    // CDRs must remain in DB (not deleted) — bill_id is set by the export
    // worker, not synchronously in billing_service for archive=true.
    assert!(
        wholesale_cdr::Entity::find_by_id(cdr1.id)
            .one(&db)
            .await
            .unwrap()
            .is_some(),
        "CDR 1 must still exist after archive=true (async path)"
    );
    assert!(
        wholesale_cdr::Entity::find_by_id(cdr2.id)
            .one(&db)
            .await
            .unwrap()
            .is_some(),
        "CDR 2 must still exist after archive=true (async path)"
    );

    // unbilled summary is still non-zero: CDRs are not linked until the worker runs
    let unbilled_after = service.get_unbilled_summary(tenant.id).await.unwrap();
    assert_eq!(
        unbilled_after, 30.0,
        "CDRs are not linked yet; worker links them"
    );

    // An async export task must have been created
    let tasks = export_task::Entity::find()
        .filter(export_task::Column::TaskType.eq("billing_archive"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "one billing_archive export task must be created"
    );
    let filters: serde_json::Value =
        serde_json::from_str(tasks[0].filters.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(
        filters["bill_id"].as_i64(),
        Some(bill.id),
        "export task filters must contain the correct bill_id"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 1b. archive=true → CDRs are linked via bill_id, export task is enqueued
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_archive_links_cdrs_and_creates_export_task() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Archive Async Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    let cdr = insert_cdr(
        &db,
        tenant.id,
        "async-call-1",
        5.0,
        30,
        true,
        now - Duration::hours(1),
    )
    .await;

    let bill = service
        .generate_bill_for_tenant(
            tenant.id,
            now - Duration::days(1),
            now + Duration::days(1),
            true,
        )
        .await
        .unwrap();

    // bill.file_path is not set yet (async worker handles that)
    assert!(
        bill.file_path.is_none(),
        "file_path must be None until worker runs"
    );

    // CDR is still in DB; bill_id is set by the export worker (not synchronously)
    let found = wholesale_cdr::Entity::find_by_id(cdr.id)
        .one(&db)
        .await
        .unwrap()
        .expect("CDR must remain in DB");
    assert_eq!(
        found.bill_id, None,
        "CDR bill_id is set by worker, not billing_service for archive=true"
    );

    // An export task was created
    let task = export_task::Entity::find()
        .filter(export_task::Column::TaskType.eq("billing_archive"))
        .filter(export_task::Column::Status.eq("pending"))
        .one(&db)
        .await
        .unwrap()
        .expect("a billing_archive export task must be pending");
    let filters: serde_json::Value =
        serde_json::from_str(task.filters.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(filters["bill_id"].as_i64(), Some(bill.id));
    // Worker-linking fields must also be present in filters
    assert!(
        filters["tenant_id"].as_i64().is_some(),
        "filters must contain tenant_id for worker CDR linking"
    );
    assert!(
        filters["period_start"].as_str().is_some(),
        "filters must contain period_start"
    );
    assert!(
        filters["period_end"].as_str().is_some(),
        "filters must contain period_end"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 1c. archive=false → CDRs remain in DB with bill_id set
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_no_archive_keeps_cdrs_linked_in_db() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "No Archive Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    let cdr = insert_cdr(
        &db,
        tenant.id,
        "keep-call-1",
        7.0,
        45,
        true,
        now - Duration::hours(1),
    )
    .await;

    let bill = service
        .generate_bill_for_tenant(
            tenant.id,
            now - Duration::days(1),
            now + Duration::days(1),
            false,
        )
        .await
        .unwrap();

    assert!(bill.file_path.is_none(), "no archive → no file");

    // CDR must still be in DB, linked to the bill
    let found = wholesale_cdr::Entity::find_by_id(cdr.id)
        .one(&db)
        .await
        .unwrap()
        .expect("CDR must remain in DB when archive=false");
    assert_eq!(found.bill_id, Some(bill.id), "CDR must be linked to bill");
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Idempotency: same (tenant, period_start, period_end) → same bill returned
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_generate_bill_idempotent_exact_timestamps() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Idempotent Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    insert_cdr(
        &db,
        tenant.id,
        "call-idem",
        5.0,
        30,
        false,
        now - Duration::hours(1),
    )
    .await;

    let start = now - Duration::days(1);
    let end = now + Duration::days(1);

    let bill1 = service
        .generate_bill_for_tenant(tenant.id, start, end, false)
        .await
        .unwrap();
    let bill2 = service
        .generate_bill_for_tenant(tenant.id, start, end, false)
        .await
        .unwrap();

    assert_eq!(
        bill1.id, bill2.id,
        "same bill must be returned on second call"
    );

    let all_bills = bill::Entity::find()
        .filter(bill::Column::TenantId.eq(tenant.id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(all_bills.len(), 1, "only one bill should exist");
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Idempotency via bill_number collision (different timestamps, same date)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_generate_bill_idempotent_by_bill_number() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "BillNum Tenant").await;
    let service = make_service(db.clone(), &tmp);

    // Use a fixed date so bill_number is deterministic
    let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

    insert_cdr(
        &db,
        tenant.id,
        "call-bn",
        7.0,
        45,
        false,
        base + Duration::hours(1),
    )
    .await;

    // First call with start=base+0s, end=base+24h
    let bill1 = service
        .generate_bill_for_tenant(tenant.id, base, base + Duration::days(1), false)
        .await
        .unwrap();

    // Second call with start=base+1s (different timestamp, same date → same bill_number)
    let bill2 = service
        .generate_bill_for_tenant(
            tenant.id,
            base + Duration::seconds(1),
            base + Duration::days(1),
            false,
        )
        .await
        .unwrap();

    assert_eq!(
        bill1.id, bill2.id,
        "should return existing bill by bill_number"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Empty period: no CDRs → bill created with zero amounts
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_generate_bill_empty_period() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Empty Period Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    let bill = service
        .generate_bill_for_tenant(tenant.id, now - Duration::days(1), now, false)
        .await
        .expect("should succeed with 0 CDRs");

    assert_eq!(bill.total_amount, 0.0);
    assert_eq!(bill.call_count, 0);
    assert_eq!(bill.total_duration, 0);
    assert_eq!(bill.status, "Draft");
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. CDRs outside the billing period are excluded
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_cdrs_outside_period_excluded() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Outside Period Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    let period_start = now - Duration::days(3);
    let period_end = now - Duration::days(1);

    // inside period
    insert_cdr(
        &db,
        tenant.id,
        "inside",
        50.0,
        300,
        true,
        now - Duration::days(2),
    )
    .await;
    // outside (too old)
    insert_cdr(
        &db,
        tenant.id,
        "before",
        99.0,
        600,
        true,
        now - Duration::days(5),
    )
    .await;
    // outside (after end)
    insert_cdr(&db, tenant.id, "after", 99.0, 600, true, now).await;

    let bill = service
        .generate_bill_for_tenant(tenant.id, period_start, period_end, false)
        .await
        .unwrap();

    assert_eq!(bill.total_amount, 50.0);
    assert_eq!(bill.call_count, 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. CDRs already bound to a previous bill are excluded from the new one
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_already_billed_cdrs_excluded() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Already Billed Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    let start = now - Duration::days(1);
    let end = now + Duration::days(1);

    // First bill: 1 CDR = $10
    insert_cdr(
        &db,
        tenant.id,
        "call-old",
        10.0,
        60,
        true,
        now - Duration::hours(3),
    )
    .await;
    let bill1 = service
        .generate_bill_for_tenant(tenant.id, start, end, false)
        .await
        .unwrap();
    assert_eq!(bill1.total_amount, 10.0);

    // New CDR arrives after first bill
    insert_cdr(
        &db,
        tenant.id,
        "call-new",
        25.0,
        90,
        true,
        now - Duration::minutes(30),
    )
    .await;

    // Second bill for a different date range to avoid bill_number collision
    let start2 = now - Duration::days(2);
    let bill2 = service
        .generate_bill_for_tenant(tenant.id, start2, end, false)
        .await
        .unwrap();

    // Only the new unbilled CDR ($25) should be included
    assert_eq!(bill2.total_amount, 25.0);
    assert_eq!(bill2.call_count, 1);
    assert_ne!(bill1.id, bill2.id);
}

// ─────────────────────────────────────────────────────────────────────────────
// 7. ASR and ring_ASR calculation
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_asr_calculation() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "ASR Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    // 3 calls: 2 answered, 1 not
    insert_cdr(
        &db,
        tenant.id,
        "asr-1",
        1.0,
        30,
        true,
        now - Duration::hours(3),
    )
    .await;
    insert_cdr(
        &db,
        tenant.id,
        "asr-2",
        1.0,
        30,
        true,
        now - Duration::hours(2),
    )
    .await;
    insert_cdr(
        &db,
        tenant.id,
        "asr-3",
        1.0,
        0,
        false,
        now - Duration::hours(1),
    )
    .await;

    let bill = service
        .generate_bill_for_tenant(
            tenant.id,
            now - Duration::days(1),
            now + Duration::days(1),
            false,
        )
        .await
        .unwrap();

    assert_eq!(bill.call_count, 3);
    let expected_asr = 2.0 / 3.0;
    assert!(
        (bill.asr - expected_asr).abs() < 1e-9,
        "asr={} expected={}",
        bill.asr,
        expected_asr
    );
    assert!((bill.ring_asr - expected_asr).abs() < 1e-9);
}

// ─────────────────────────────────────────────────────────────────────────────
// 8. Multi-tenant isolation: billing tenant A does not affect tenant B
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_multi_tenant_isolation() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let ta = create_tenant(&db, "Tenant A").await;
    let tb = create_tenant(&db, "Tenant B").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    insert_cdr(
        &db,
        ta.id,
        "a-call",
        100.0,
        300,
        true,
        now - Duration::hours(1),
    )
    .await;
    insert_cdr(
        &db,
        tb.id,
        "b-call",
        99.0,
        300,
        true,
        now - Duration::hours(1),
    )
    .await;

    let start = now - Duration::days(1);
    let end = now + Duration::days(1);

    let bill_a = service
        .generate_bill_for_tenant(ta.id, start, end, false)
        .await
        .unwrap();

    let unbilled_b = service.get_unbilled_summary(tb.id).await.unwrap();
    assert_eq!(
        unbilled_b, 99.0,
        "tenant B CDR must not be touched by tenant A billing"
    );
    assert_eq!(bill_a.total_amount, 100.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// 9. run_billing_cycle: no bill_settings → no bill created
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_run_billing_cycle_no_settings() {
    let db = setup_db().await;
    let tenant = create_tenant(&db, "No Settings Tenant").await;
    let now = Utc::now();
    let service = BillingService::new(db.clone());

    insert_cdr(
        &db,
        tenant.id,
        "cycle-no-setting",
        10.0,
        60,
        true,
        now - Duration::hours(1),
    )
    .await;

    service
        .run_billing_cycle()
        .await
        .expect("run_billing_cycle should not error");

    let bills = bill::Entity::find()
        .filter(bill::Column::TenantId.eq(tenant.id))
        .all(&db)
        .await
        .unwrap();
    assert!(
        bills.is_empty(),
        "no bill should be created without bill settings"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 10. run_billing_cycle: wrong settlement_day → no bill
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_run_billing_cycle_wrong_day() {
    let db = setup_db().await;
    let tenant = create_tenant(&db, "Wrong Day Tenant").await;
    let service = BillingService::new(db.clone());
    let now = Utc::now();
    let local_now = now.with_timezone(&chrono::Local);

    // Pick a settlement_day that is never today
    let today_day = local_now.day() as i32;
    let wrong_day = if today_day == 1 { 2 } else { 1 };

    bill_setting::ActiveModel {
        tenant_id: Set(tenant.id),
        settlement_cycle: Set("Monthly".to_string()),
        settlement_day: Set(wrong_day),
        archive_enabled: Set(false),
        archive_cycle: Set("Monthly".to_string()),
        archive_day: Set(1),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    insert_cdr(
        &db,
        tenant.id,
        "wd-call",
        5.0,
        30,
        false,
        now - Duration::hours(1),
    )
    .await;

    service.run_billing_cycle().await.unwrap();

    let bills = bill::Entity::find()
        .filter(bill::Column::TenantId.eq(tenant.id))
        .all(&db)
        .await
        .unwrap();
    assert!(
        bills.is_empty(),
        "no bill should be created on wrong settlement day"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 11. run_billing_cycle: correct settlement_day → bill created
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_run_billing_cycle_correct_day() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Correct Day Tenant").await;
    let service = make_service(db.clone(), &tmp);
    let now = Utc::now();
    let local_now = now.with_timezone(&chrono::Local);

    let today_day = local_now.day() as i32;

    bill_setting::ActiveModel {
        tenant_id: Set(tenant.id),
        settlement_cycle: Set("Monthly".to_string()),
        settlement_day: Set(today_day),
        archive_enabled: Set(false),
        archive_cycle: Set("Monthly".to_string()),
        archive_day: Set(1),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    insert_cdr(
        &db,
        tenant.id,
        "cd-call",
        15.0,
        90,
        true,
        now - Duration::days(1) - Duration::hours(1),
    )
    .await;

    service.run_billing_cycle().await.unwrap();

    let bills = bill::Entity::find()
        .filter(bill::Column::TenantId.eq(tenant.id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(
        bills.len(),
        1,
        "one bill should be created on correct settlement day"
    );
    assert_eq!(bills[0].call_count, 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// 12. Archive async task: verify export_task fields
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_archive_export_task_has_correct_bills_dir() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Bills Dir Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    insert_cdr(
        &db,
        tenant.id,
        "csv-call-1",
        3.14,
        77,
        true,
        now - Duration::hours(2),
    )
    .await;

    let bill = service
        .generate_bill_for_tenant(
            tenant.id,
            now - Duration::days(1),
            now + Duration::days(1),
            true,
        )
        .await
        .unwrap();

    // Export task should have been created with the configured bills_dir
    let task = export_task::Entity::find()
        .filter(export_task::Column::TaskType.eq("billing_archive"))
        .one(&db)
        .await
        .unwrap()
        .expect("billing_archive task must exist");

    let filters: serde_json::Value =
        serde_json::from_str(task.filters.as_deref().unwrap_or("{}")).unwrap();

    assert_eq!(filters["bill_id"].as_i64(), Some(bill.id));
    // bills_dir should be embedded in the task filters
    let bills_dir_in_task = filters["bills_dir"].as_str().unwrap_or("");
    assert!(
        !bills_dir_in_task.is_empty(),
        "bills_dir must be present in task filters"
    );
    assert_eq!(
        bills_dir_in_task,
        tmp.path().to_str().unwrap(),
        "bills_dir in task must match the service's configured directory"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 13. ring_asr vs asr: calls that ring but are never answered
// ─────────────────────────────────────────────────────────────────────────────

/// Helper: insert a CDR that has ring_time but no answer_time (ringing, unanswered).
async fn insert_ringing_cdr(
    db: &DatabaseConnection,
    tenant_id: i64,
    call_id: &str,
    price: f64,
    created_at: chrono::DateTime<Utc>,
) -> wholesale_cdr::Model {
    wholesale_cdr::ActiveModel {
        tenant_id: Set(tenant_id),
        call_id: Set(call_id.to_string()),
        price_total: Set(price),
        duration: Set(0),
        status: Set("no-answer".to_string()),
        status_code: Set(Some(486)),
        caller: Set("100".to_string()),
        callee: Set("200".to_string()),
        created_at: Set(created_at),
        ring_time: Set(Some(created_at + Duration::seconds(3))),
        answer_time: Set(None),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap_or_else(|e| panic!("insert_ringing_cdr '{}': {}", call_id, e))
}

#[tokio::test]
async fn test_ring_asr_higher_than_asr_when_calls_ring_but_not_answered() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Ring ASR Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    // 1 fully answered call
    insert_cdr(
        &db,
        tenant.id,
        "ring-ans-1",
        1.0,
        60,
        true,
        now - Duration::hours(3),
    )
    .await;
    // 1 ringing-only call (ring_time set, no answer_time)
    insert_ringing_cdr(&db, tenant.id, "ring-only-1", 0.0, now - Duration::hours(2)).await;
    // 1 completely unanswered call (no ring, no answer)
    insert_cdr(
        &db,
        tenant.id,
        "no-ring-1",
        0.0,
        0,
        false,
        now - Duration::hours(1),
    )
    .await;

    let bill = service
        .generate_bill_for_tenant(
            tenant.id,
            now - Duration::days(1),
            now + Duration::days(1),
            false,
        )
        .await
        .unwrap();

    assert_eq!(bill.call_count, 3);
    // ASR: 1 answered / 3 total = 0.333...
    let expected_asr = 1.0 / 3.0;
    // ring_ASR: 2 with ring_time / 3 total = 0.666...
    let expected_ring_asr = 2.0 / 3.0;

    assert!(
        (bill.asr - expected_asr).abs() < 1e-9,
        "asr={} expected={}",
        bill.asr,
        expected_asr
    );
    assert!(
        (bill.ring_asr - expected_ring_asr).abs() < 1e-9,
        "ring_asr={} expected={}",
        bill.ring_asr,
        expected_ring_asr
    );
    assert!(
        bill.ring_asr > bill.asr,
        "ring_asr must be greater than asr when some calls ring but are not answered"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 14. Bill number format: BILL-{tenant_id}-{YYYYMMDD}
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_bill_number_format() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Bill Number Tenant").await;
    let service = make_service(db.clone(), &tmp);

    // Use a fixed start time so the date is predictable
    let start = chrono::Utc.with_ymd_and_hms(2026, 3, 15, 0, 0, 0).unwrap();
    let end = start + Duration::days(1);

    let bill = service
        .generate_bill_for_tenant(tenant.id, start, end, false)
        .await
        .unwrap();

    let expected = format!("BILL-{}-20260315", tenant.id);
    assert_eq!(
        bill.bill_number, expected,
        "bill_number must follow the BILL-{{tenant_id}}-{{YYYYMMDD}} format"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 15. get_unbilled_summary: only counts CDRs of the requested tenant
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_unbilled_summary_multi_tenant_isolation() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let ta = create_tenant(&db, "Summary Tenant A").await;
    let tb = create_tenant(&db, "Summary Tenant B").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    insert_cdr(
        &db,
        ta.id,
        "sum-a-1",
        10.0,
        60,
        true,
        now - Duration::hours(2),
    )
    .await;
    insert_cdr(
        &db,
        ta.id,
        "sum-a-2",
        20.0,
        120,
        false,
        now - Duration::hours(1),
    )
    .await;
    insert_cdr(
        &db,
        tb.id,
        "sum-b-1",
        999.0,
        300,
        true,
        now - Duration::hours(1),
    )
    .await;

    let summary_a = service.get_unbilled_summary(ta.id).await.unwrap();
    let summary_b = service.get_unbilled_summary(tb.id).await.unwrap();

    assert_eq!(summary_a, 30.0, "tenant A unbilled summary must be 30.0");
    assert_eq!(summary_b, 999.0, "tenant B unbilled summary must be 999.0");
}

// ─────────────────────────────────────────────────────────────────────────────
// 16. total_duration is the sum of all CDR durations in the period
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_total_duration_aggregation() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Duration Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    let durations = [30_i32, 60, 90, 120, 0];
    for (i, &dur) in durations.iter().enumerate() {
        insert_cdr(
            &db,
            tenant.id,
            &format!("dur-{}", i),
            1.0,
            dur,
            dur > 0,
            now - Duration::hours((i + 1) as i64),
        )
        .await;
    }

    let bill = service
        .generate_bill_for_tenant(
            tenant.id,
            now - Duration::days(1),
            now + Duration::days(1),
            false,
        )
        .await
        .unwrap();

    let expected_duration: i32 = durations.iter().sum();
    assert_eq!(
        bill.total_duration, expected_duration as i64,
        "total_duration must equal the sum of all CDR durations"
    );
    assert_eq!(bill.call_count, durations.len() as i64);
}

// ─────────────────────────────────────────────────────────────────────────────
// 17. Zero-price CDRs are counted in call_count but do not inflate total_amount
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_zero_price_cdrs_counted_but_not_in_amount() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Zero Price Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    // Two paid CDRs
    insert_cdr(
        &db,
        tenant.id,
        "paid-1",
        5.0,
        60,
        true,
        now - Duration::hours(3),
    )
    .await;
    insert_cdr(
        &db,
        tenant.id,
        "paid-2",
        10.0,
        120,
        true,
        now - Duration::hours(2),
    )
    .await;
    // Two zero-price CDRs (e.g. internal calls)
    insert_cdr(
        &db,
        tenant.id,
        "free-1",
        0.0,
        30,
        false,
        now - Duration::hours(1),
    )
    .await;
    insert_cdr(
        &db,
        tenant.id,
        "free-2",
        0.0,
        45,
        false,
        now - Duration::minutes(30),
    )
    .await;

    let bill = service
        .generate_bill_for_tenant(
            tenant.id,
            now - Duration::days(1),
            now + Duration::days(1),
            false,
        )
        .await
        .unwrap();

    assert_eq!(bill.call_count, 4, "all 4 CDRs must be counted");
    assert_eq!(
        bill.total_amount, 15.0,
        "zero-price CDRs must not inflate total_amount"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 18. Weekly billing cycle: bill is created on the correct weekday
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_run_billing_cycle_weekly_correct_weekday() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Weekly Correct Tenant").await;
    let service = make_service(db.clone(), &tmp);
    let now = Utc::now();
    let local_now = now.with_timezone(&chrono::Local);

    // Today's weekday (1=Mon … 7=Sun)
    let today_weekday = local_now.weekday().number_from_monday() as i32;

    bill_setting::ActiveModel {
        tenant_id: Set(tenant.id),
        settlement_cycle: Set("Weekly".to_string()),
        settlement_day: Set(today_weekday),
        archive_enabled: Set(false),
        archive_cycle: Set("Weekly".to_string()),
        archive_day: Set(1),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    insert_cdr(
        &db,
        tenant.id,
        "weekly-call",
        8.0,
        60,
        true,
        now - Duration::days(3),
    )
    .await;

    service.run_billing_cycle().await.unwrap();

    let bills = bill::Entity::find()
        .filter(bill::Column::TenantId.eq(tenant.id))
        .all(&db)
        .await
        .unwrap();

    assert_eq!(
        bills.len(),
        1,
        "one bill must be created on the correct weekly settlement day"
    );
    assert_eq!(bills[0].call_count, 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// 19. Weekly billing cycle: no bill on wrong weekday
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_run_billing_cycle_weekly_wrong_weekday() {
    let db = setup_db().await;
    let tenant = create_tenant(&db, "Weekly Wrong Tenant").await;
    let service = BillingService::new(db.clone());
    let now = Utc::now();
    let local_now = now.with_timezone(&chrono::Local);

    let today_weekday = local_now.weekday().number_from_monday() as i32;
    // Pick a different weekday (wrap 1..=7)
    let wrong_weekday = (today_weekday % 7) + 1;

    bill_setting::ActiveModel {
        tenant_id: Set(tenant.id),
        settlement_cycle: Set("Weekly".to_string()),
        settlement_day: Set(wrong_weekday),
        archive_enabled: Set(false),
        archive_cycle: Set("Weekly".to_string()),
        archive_day: Set(1),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    insert_cdr(
        &db,
        tenant.id,
        "ww-call",
        5.0,
        30,
        false,
        now - Duration::hours(1),
    )
    .await;

    service.run_billing_cycle().await.unwrap();

    let bills = bill::Entity::find()
        .filter(bill::Column::TenantId.eq(tenant.id))
        .all(&db)
        .await
        .unwrap();

    assert!(
        bills.is_empty(),
        "no bill must be created on the wrong weekly settlement day"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 20. get_unbilled_summary returns 0 when there are no CDRs
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_unbilled_summary_no_cdrs() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Empty Summary Tenant").await;
    let service = make_service(db.clone(), &tmp);

    let summary = service.get_unbilled_summary(tenant.id).await.unwrap();
    assert_eq!(summary, 0.0, "unbilled summary must be 0.0 with no CDRs");
}

// ─────────────────────────────────────────────────────────────────────────────
// 21. generate_bill_for_tenant: bill status is always "Draft" on creation
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_generated_bill_has_draft_status() {
    let db = setup_db().await;
    let tmp = TempDir::new().unwrap();
    let tenant = create_tenant(&db, "Draft Status Tenant").await;
    let now = Utc::now();
    let service = make_service(db.clone(), &tmp);

    insert_cdr(
        &db,
        tenant.id,
        "draft-call",
        1.0,
        60,
        true,
        now - Duration::hours(1),
    )
    .await;

    let bill = service
        .generate_bill_for_tenant(
            tenant.id,
            now - Duration::days(1),
            now + Duration::days(1),
            false,
        )
        .await
        .unwrap();

    assert_eq!(
        bill.status, "Draft",
        "newly created bill must have 'Draft' status"
    );
    assert_eq!(
        bill.total_amount, bill.actual_amount,
        "total_amount and actual_amount must be equal on creation"
    );
}

