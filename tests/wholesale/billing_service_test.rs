    use rustpbx::addons::wholesale::billing_service::BillingService;
    use rustpbx::addons::wholesale::migration::Migrator as WholesaleMigrator;
    use rustpbx::addons::wholesale::models::{trunk_daily_stats, wholesale_cdr};
    use rustpbx::models::migration::Migrator as MainMigrator;
    use chrono::{Duration, Utc};
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait,
        QueryFilter, Set,
    };
    use sea_orm_migration::MigratorTrait;

    async fn setup_db() -> DatabaseConnection {
        use rustpbx::addons::wholesale::models::tenant;

        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");

        MainMigrator::up(&db, None).await.expect("main migrations");
        WholesaleMigrator::up(&db, None)
            .await
            .expect("wholesale migrations");

        // Create a default tenant for foreign key constraints
        tenant::ActiveModel {
            id: Set(1),
            name: Set("Test Tenant".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("create test tenant");

        db
    }

    /// Test that generate_daily_stats_for_period creates correct stats from CDRs
    #[tokio::test]
    async fn test_generate_daily_stats_creates_records() {
        let db = setup_db().await;
        let service = BillingService::new(db.clone());

        // Create test CDRs for carrier_id=1 on two different days
        let now = Utc::now();
        let day1 = now - Duration::days(2);
        let day2 = now - Duration::days(1);

        // Day 1: 3 calls, 2 answered
        for i in 0..3 {
            let mut cdr = create_test_cdr(1, day1 + Duration::seconds(i * 60));
            if i < 2 {
                cdr.answer_time = Set(Some(day1 + Duration::seconds(5)));
            }
            cdr.insert(&db).await.unwrap();
        }

        // Day 2: 2 calls, 1 answered
        for i in 0..2 {
            let mut cdr = create_test_cdr(1, day2 + Duration::seconds(i * 60));
            if i == 0 {
                cdr.answer_time = Set(Some(day2 + Duration::seconds(5)));
            }
            cdr.insert(&db).await.unwrap();
        }

        // Generate stats
        let start = (now - Duration::days(3))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        let count = service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();
        assert_eq!(count, 2, "Should create 2 daily stat records");

        // Verify stats were created
        let stats = trunk_daily_stats::Entity::find()
            .filter(trunk_daily_stats::Column::CarrierId.eq(1))
            .all(&db)
            .await
            .unwrap();

        assert_eq!(stats.len(), 2);

        // Find day1 stats
        let day1_stats = stats.iter().find(|s| s.date == day1.date_naive()).unwrap();
        assert_eq!(day1_stats.total_calls, 3);
        assert_eq!(day1_stats.answered_calls, 2);

        // Find day2 stats
        let day2_stats = stats.iter().find(|s| s.date == day2.date_naive()).unwrap();
        assert_eq!(day2_stats.total_calls, 2);
        assert_eq!(day2_stats.answered_calls, 1);
    }

    /// Test that generate_daily_stats_for_period updates existing records (upsert)
    #[tokio::test]
    async fn test_generate_daily_stats_updates_existing() {
        let db = setup_db().await;
        let service = BillingService::new(db.clone());

        let now = Utc::now();
        let day = (now - Duration::days(1))
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();

        // Create initial CDR
        let cdr = create_test_cdr(1, day);
        cdr.insert(&db).await.unwrap();

        // Generate stats first time
        let start = (now - Duration::days(2))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        let count = service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Add another CDR for same day
        let cdr2 = create_test_cdr(1, day + Duration::minutes(30));
        cdr2.insert(&db).await.unwrap();

        // Generate stats again (should update, not create new)
        let count2 = service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();
        assert_eq!(count2, 1, "Should still be 1 record (updated)");

        // Verify the stats were updated
        let stats = trunk_daily_stats::Entity::find()
            .filter(trunk_daily_stats::Column::CarrierId.eq(1))
            .filter(trunk_daily_stats::Column::Date.eq(day.date_naive()))
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(stats.total_calls, 2, "Should now have 2 calls");
    }

    /// Test that CDRs without carrier_id are excluded
    #[tokio::test]
    async fn test_generate_daily_stats_excludes_null_carrier() {
        let db = setup_db().await;
        let service = BillingService::new(db.clone());

        let now = Utc::now();
        let day = now - Duration::days(1);

        // Create CDR with null carrier_id
        let mut cdr = create_test_cdr(1, day);
        cdr.carrier_id = Set(None);
        cdr.insert(&db).await.unwrap();

        let start = (now - Duration::days(2))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        let count = service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();
        assert_eq!(count, 0, "Should not create stats for null carrier_id");
    }

    /// Test aggregation of cost, price, and profit
    #[tokio::test]
    async fn test_generate_daily_stats_aggregates_financials() {
        let db = setup_db().await;
        let service = BillingService::new(db.clone());

        let now = Utc::now();
        let day = now - Duration::days(1);

        // Create CDRs with different costs/prices
        for i in 0..3 {
            let mut cdr = create_test_cdr(1, day + Duration::seconds(i * 60));
            cdr.cost_total = Set(0.01 * (i + 1) as f64);
            cdr.price_total = Set(0.02 * (i + 1) as f64);
            cdr.profit = Set(0.01 * (i + 1) as f64);
            cdr.insert(&db).await.unwrap();
        }

        let start = (now - Duration::days(2))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();

        let stats = trunk_daily_stats::Entity::find()
            .filter(trunk_daily_stats::Column::CarrierId.eq(1))
            .filter(trunk_daily_stats::Column::Date.eq(day.date_naive()))
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        // Sum: 0.01 + 0.02 + 0.03 = 0.06
        assert!((stats.cost_total - 0.06).abs() < 0.001);
        // Sum: 0.02 + 0.04 + 0.06 = 0.12
        assert!((stats.price_total - 0.12).abs() < 0.001);
        // Sum: 0.01 + 0.02 + 0.03 = 0.06
        assert!((stats.profit - 0.06).abs() < 0.001);
    }

    /// Test multiple carriers get separate stats
    #[tokio::test]
    async fn test_generate_daily_stats_separates_carriers() {
        let db = setup_db().await;
        let service = BillingService::new(db.clone());

        let now = Utc::now();
        let day = now - Duration::days(1);

        // Create CDRs for outbound 1
        for i in 0..2 {
            let cdr = create_test_cdr(1, day + Duration::seconds(i * 60));
            cdr.insert(&db).await.unwrap();
        }

        // Create CDRs for outbound 2
        for i in 0..3 {
            let cdr = create_test_cdr(2, day + Duration::seconds(i * 60));
            cdr.insert(&db).await.unwrap();
        }

        let start = (now - Duration::days(2))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        let count = service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();
        assert_eq!(count, 2, "Should create 2 records (one per outbound)");

        let carrier1_stats = trunk_daily_stats::Entity::find()
            .filter(trunk_daily_stats::Column::CarrierId.eq(1))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(carrier1_stats.total_calls, 2);

        let carrier2_stats = trunk_daily_stats::Entity::find()
            .filter(trunk_daily_stats::Column::CarrierId.eq(2))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(carrier2_stats.total_calls, 3);
    }

    /// Test that stats are preserved after CDR archival (simulate archive by deleting CDRs)
    #[tokio::test]
    async fn test_stats_preserved_after_cdr_archive() {
        let db = setup_db().await;
        let billing_service = BillingService::new(db.clone());
        let stats_service = rustpbx::addons::wholesale::stats_service::StatsService::new(db.clone());

        let now = Utc::now();
        let yesterday = (now - Duration::days(1))
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();

        // Create CDRs for yesterday (will be archived)
        for i in 0..5 {
            let mut cdr = create_test_cdr(1, yesterday + Duration::seconds(i * 60));
            if i < 3 {
                cdr.answer_time = Set(Some(yesterday + Duration::seconds(5)));
            }
            cdr.insert(&db).await.unwrap();
        }

        // Generate daily stats BEFORE archival
        let start = (now - Duration::days(2))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        let count = billing_service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();
        assert_eq!(count, 1, "Should create 1 daily stat record");

        // Verify stats exist in trunk_daily_stats
        let stats_before = trunk_daily_stats::Entity::find()
            .filter(trunk_daily_stats::Column::CarrierId.eq(1))
            .filter(trunk_daily_stats::Column::Date.eq(yesterday.date_naive()))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats_before.total_calls, 5);
        assert_eq!(stats_before.answered_calls, 3);

        // Simulate archival: delete all CDRs for yesterday
        let delete_result = wholesale_cdr::Entity::delete_many()
            .filter(wholesale_cdr::Column::CarrierId.eq(1))
            .exec(&db)
            .await
            .unwrap();
        assert_eq!(delete_result.rows_affected, 5, "Should delete 5 CDRs");

        // Verify CDRs are gone
        let cdr_count = wholesale_cdr::Entity::find()
            .filter(wholesale_cdr::Column::CarrierId.eq(1))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(cdr_count, 0, "CDRs should be deleted");

        // Query stats using StatsService - should still return data from trunk_daily_stats
        let from = (now - Duration::days(2)).date_naive();
        let to = now.date_naive();
        let daily_stats = stats_service
            .get_carrier_daily_stats(1, from, to)
            .await
            .unwrap();

        // Should have 1 day of stats from historical data
        assert_eq!(
            daily_stats.len(),
            1,
            "Should have 1 day of historical stats"
        );
        assert_eq!(
            daily_stats[0].total_calls, 5,
            "Historical calls should be preserved"
        );
        assert_eq!(
            daily_stats[0].answered_calls, 3,
            "Historical answered should be preserved"
        );
        assert_eq!(daily_stats[0].source, Some("historical".to_string()));
    }

    /// Test that StatsService merges historical and realtime data correctly
    #[tokio::test]
    async fn test_stats_service_merges_historical_and_realtime() {
        let db = setup_db().await;
        let billing_service = BillingService::new(db.clone());
        let stats_service = rustpbx::addons::wholesale::stats_service::StatsService::new(db.clone());

        let now = Utc::now();
        let yesterday = now - Duration::days(1);

        // Create CDRs for yesterday and generate historical stats
        for i in 0..3 {
            let mut cdr = create_test_cdr(1, yesterday + Duration::seconds(i * 60));
            if i < 2 {
                cdr.answer_time = Set(Some(yesterday + Duration::seconds(5)));
            }
            cdr.insert(&db).await.unwrap();
        }

        let start = (now - Duration::days(2))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        billing_service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();

        // Create CDRs for today (realtime data)
        for i in 0..2 {
            let mut cdr = create_test_cdr(1, now - Duration::seconds(i * 60));
            cdr.answer_time = Set(Some(now - Duration::seconds(i * 60 - 5)));
            cdr.insert(&db).await.unwrap();
        }

        // Query stats for both days
        let from = yesterday.date_naive();
        let to = now.date_naive();
        let daily_stats = stats_service
            .get_carrier_daily_stats(1, from, to)
            .await
            .unwrap();

        assert_eq!(daily_stats.len(), 2, "Should have 2 days of stats");

        // Yesterday should be from historical
        let yesterday_stats = daily_stats
            .iter()
            .find(|s| s.date == yesterday.date_naive())
            .unwrap();
        assert_eq!(yesterday_stats.total_calls, 3);
        assert_eq!(yesterday_stats.source, Some("historical".to_string()));

        // Today should be from realtime
        let today_stats = daily_stats
            .iter()
            .find(|s| s.date == now.date_naive())
            .unwrap();
        assert_eq!(today_stats.total_calls, 2);
        assert_eq!(today_stats.source, Some("realtime".to_string()));
    }

    /// Test summary stats aggregation from both historical and realtime sources
    #[tokio::test]
    async fn test_stats_service_summary_aggregation() {
        let db = setup_db().await;
        let billing_service = BillingService::new(db.clone());
        let stats_service = rustpbx::addons::wholesale::stats_service::StatsService::new(db.clone());

        let now = Utc::now();
        let yesterday = now - Duration::days(1);

        // Create historical data for yesterday
        for i in 0..4 {
            let mut cdr = create_test_cdr(1, yesterday + Duration::seconds(i * 60));
            cdr.price_total = Set(0.10);
            cdr.cost_total = Set(0.05);
            cdr.profit = Set(0.05);
            if i < 3 {
                cdr.answer_time = Set(Some(yesterday + Duration::seconds(5)));
            }
            cdr.insert(&db).await.unwrap();
        }

        let start = (now - Duration::days(2))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();

        billing_service
            .generate_daily_stats_for_period(start, end)
            .await
            .unwrap();

        // Create realtime data for today
        for i in 0..2 {
            let mut cdr = create_test_cdr(1, now - Duration::seconds(i * 60));
            cdr.price_total = Set(0.20);
            cdr.cost_total = Set(0.10);
            cdr.profit = Set(0.10);
            cdr.answer_time = Set(Some(now - Duration::seconds(i * 60 - 5)));
            cdr.insert(&db).await.unwrap();
        }

        // Get summary stats for both days
        let from = yesterday.date_naive();
        let to = now.date_naive();
        let summary = stats_service
            .get_carrier_summary_stats(1, from, to)
            .await
            .unwrap();

        // Total: 4 (yesterday) + 2 (today) = 6
        assert_eq!(summary.total_calls, 6, "Total calls should be 6");
        // Answered: 3 (yesterday) + 2 (today) = 5
        assert_eq!(summary.answered_calls, 5, "Answered calls should be 5");
        // Revenue: 0.10 * 4 + 0.20 * 2 = 0.80
        assert!(
            (summary.price_total - 0.80).abs() < 0.001,
            "Revenue should be 0.80"
        );
        // Cost: 0.05 * 4 + 0.10 * 2 = 0.40
        assert!(
            (summary.cost_total - 0.40).abs() < 0.001,
            "Cost should be 0.40"
        );
        // Profit: 0.05 * 4 + 0.10 * 2 = 0.40
        assert!(
            (summary.profit - 0.40).abs() < 0.001,
            "Profit should be 0.40"
        );
        // ASR: 5/6 = 83.33%
        assert!((summary.asr - 83.33).abs() < 0.1, "ASR should be ~83.33%");
    }

    /// Helper to create a test CDR
    fn create_test_cdr(
        carrier_id: i64,
        created_at: chrono::DateTime<Utc>,
    ) -> wholesale_cdr::ActiveModel {
        wholesale_cdr::ActiveModel {
            call_id: Set(format!(
                "test-call-{}-{}",
                carrier_id,
                created_at.timestamp()
            )),
            tenant_id: Set(1),
            carrier_id: Set(Some(carrier_id)),
            duration: Set(60),
            status: Set("answered".to_string()),
            status_code: Set(Some(200)),
            caller: Set("1001".to_string()),
            callee: Set("12345678".to_string()),
            tenant_rate: Set(0.02),
            tenant_min_duration: Set(60),
            tenant_increment: Set(60),
            price_total: Set(0.02),
            vendor_rate: Set(0.01),
            vendor_min_duration: Set(60),
            vendor_increment: Set(60),
            cost_total: Set(0.01),
            profit: Set(0.01),
            created_at: Set(created_at),
            ..Default::default()
        }
    }

