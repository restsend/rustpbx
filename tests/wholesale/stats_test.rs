    use rustpbx::addons::wholesale::handlers::get_tenant_daily_stats;
    use rustpbx::addons::wholesale::migration::Migrator as WholesaleMigrator;
    use rustpbx::addons::wholesale::models::{tenant, wholesale_cdr};
    use rustpbx::models::migration::Migrator as MainMigrator;
    use rustpbx::models::{call_record, sip_trunk};
    use chrono::{Duration, Utc};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

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
    async fn test_get_tenant_daily_stats() {
        let db = setup_db().await;

        // 1. Insert some call records
        let now = Utc::now();
        let yesterday = now - Duration::days(1);
        let trunk_id = 1;

        // 0. Insert Tenant
        tenant::ActiveModel {
            id: Set(1),
            name: Set("Test Tenant".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert tenant");

        // 0. Insert SIP Trunk
        sip_trunk::ActiveModel {
            id: Set(trunk_id),
            name: Set("Test Trunk".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert trunk");

        // Record 1: Yesterday, Answered, Cost 1.0
        call_record::ActiveModel {
            call_id: Set("call-1".to_string()),
            direction: Set("outbound".to_string()),
            sip_trunk_id: Set(Some(trunk_id)),
            started_at: Set(yesterday),
            status: Set("answered".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert record 1");

        wholesale_cdr::ActiveModel {
            call_id: Set("call-1".to_string()),
            tenant_id: Set(1),
            price_total: Set(1.0),
            status: Set("answered".to_string()),
            status_code: Set(Some(200)),
            created_at: Set(yesterday),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert w_cdr 1");

        // Record 2: Yesterday, Failed, Cost 0.0
        call_record::ActiveModel {
            call_id: Set("call-2".to_string()),
            direction: Set("outbound".to_string()),
            sip_trunk_id: Set(Some(trunk_id)),
            started_at: Set(yesterday),
            status: Set("failed".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert record 2");

        // Record 3: Today, Answered, Cost 2.0
        call_record::ActiveModel {
            call_id: Set("call-3".to_string()),
            direction: Set("outbound".to_string()),
            sip_trunk_id: Set(Some(trunk_id)),
            started_at: Set(now),
            status: Set("answered".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert record 3");

        wholesale_cdr::ActiveModel {
            call_id: Set("call-3".to_string()),
            tenant_id: Set(1),
            price_total: Set(2.0),
            status: Set("answered".to_string()),
            status_code: Set(Some(200)),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert w_cdr 3");

        // Record 4: Old record (should be ignored)
        call_record::ActiveModel {
            call_id: Set("call-4".to_string()),
            direction: Set("outbound".to_string()),
            sip_trunk_id: Set(Some(trunk_id)),
            started_at: Set(now - Duration::days(30)),
            status: Set("answered".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert record 4");

        // 2. Call the function
        let recent_since = now - Duration::days(14);
        let (daily_stats, trunk_costs) = get_tenant_daily_stats(&db, &[trunk_id], recent_since)
            .await
            .expect("get stats");

        // 3. Verify results
        // We expect 2 days of stats (yesterday and today)
        assert_eq!(daily_stats.len(), 2);

        // Verify Yesterday
        let yesterday_str = yesterday.format("%Y-%m-%d").to_string();
        let stat_yesterday = daily_stats
            .iter()
            .find(|s| s.date == yesterday_str)
            .expect("yesterday stat");
        assert_eq!(stat_yesterday.total, 2);
        assert_eq!(stat_yesterday.failed, Some(1));
        assert_eq!(stat_yesterday.cost, Some(1.0));

        // Verify Today
        let today_str = now.format("%Y-%m-%d").to_string();
        let stat_today = daily_stats
            .iter()
            .find(|s| s.date == today_str)
            .expect("today stat");
        assert_eq!(stat_today.total, 1);
        assert_eq!(stat_today.failed, Some(0));
        assert_eq!(stat_today.cost, Some(2.0));

        // Verify Trunk Costs
        assert_eq!(trunk_costs.len(), 1);
        assert_eq!(trunk_costs[0].sip_trunk_id, trunk_id);
        assert_eq!(trunk_costs[0].cost, Some(3.0)); // 1.0 + 2.0
    }
