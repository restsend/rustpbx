    use rustpbx::addons::wholesale::{
        export_worker::process_task,
        handlers::CdrQuery,
        migration::Migrator as WholesaleMigrator,
        models::{export_task, tenant, wholesale_cdr},
    };
    use rustpbx::models::call_record;
    use rustpbx::models::migration::Migrator as MainMigrator;
    use async_compression::tokio::bufread::GzipDecoder;
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, Database, DatabaseConnection, EntityTrait};
    use sea_orm_migration::MigratorTrait;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;

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
    async fn test_cdr_export_process() -> anyhow::Result<()> {
        let db = setup_db().await;

        // 0. Create tenant and call record to satisfy foreign keys
        let tenant = tenant::ActiveModel {
            name: Set("Test Tenant".to_string()),
            balance: Set(100.0),
            ..Default::default()
        }
        .insert(&db)
        .await?;

        let _call_record = call_record::ActiveModel {
            call_id: Set("test-call-id".to_string()),
            direction: Set("outbound".to_string()),
            status: Set("answered".to_string()),
            started_at: Set(Utc::now()),
            duration_secs: Set(60),
            transcript_status: Set("none".to_string()),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await?;

        // 1. Create a dummy CDR
        wholesale_cdr::ActiveModel {
            call_id: Set("test-call-id".to_string()),
            tenant_id: Set(tenant.id),
            vendor_rate: Set(0.1),
            vendor_min_duration: Set(60),
            vendor_increment: Set(60),
            tenant_rate: Set(0.2),
            tenant_min_duration: Set(60),
            tenant_increment: Set(60),
            cost_total: Set(0.1),
            price_total: Set(0.2),
            profit: Set(0.1),
            duration: Set(60),
            status: Set("answered".to_string()),
            status_code: Set(Some(200)),
            caller: Set("100".to_string()),
            callee: Set("200".to_string()),
            created_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await?;

        // 2. Create export task
        let filters = CdrQuery {
            status: Some("answered".to_string()),
            ..Default::default()
        };
        let filters_json = serde_json::to_string(&filters)?;

        let task = export_task::ActiveModel {
            task_type: Set("cdr".to_string()),
            status: Set("pending".to_string()),
            filters: Set(Some(filters_json)),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&db)
        .await?;

        // 3. Process task (now writes a gzip-compressed CSV file)
        let tmp = TempDir::new()?;
        process_task(&db, task.clone(), tmp.path()).await?;

        // 4. Verify result
        let updated_task = export_task::Entity::find_by_id(task.id)
            .one(&db)
            .await?
            .unwrap();

        assert_eq!(updated_task.status, "completed");
        assert_eq!(updated_task.progress, 100);
        assert_eq!(updated_task.total_records, 1);
        // Export worker writes a gzip-compressed CSV file
        assert!(
            updated_task.file_path.is_some(),
            "file_path must be set after export"
        );
        let file_path = updated_task.file_path.as_deref().unwrap_or("");
        assert!(
            file_path.ends_with(".csv.gz"),
            "file must be a .csv.gz file"
        );

        let file = tokio::fs::File::open(file_path).await?;
        let reader = tokio::io::BufReader::new(file);
        let mut decoder = GzipDecoder::new(reader);
        let mut csv = String::new();
        decoder.read_to_string(&mut csv).await?;
        assert!(csv.contains("test-call-id"));

        Ok(())
    }
