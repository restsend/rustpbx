    use sea_orm::{ActiveModelTrait, Database, Set};
    use sea_orm_migration::MigratorTrait;
    use tempfile::TempDir;

    use rustpbx::addons::cc::CcAddonState;

    /// Create an isolated SQLite in-memory database with all CC migrations applied.
    async fn setup_db() -> sea_orm::DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        rustpbx::addons::cc::migration::Migrator::up(&db, None)
            .await
            .unwrap();
        db
    }

    /// Insert a skill group row directly into the DB.
    async fn insert_skill_group(
        db: &sea_orm::DatabaseConnection,
        id: &str,
        display_name: Option<&str>,
        skills: &[&str],
    ) {
        let now = chrono::Utc::now();
        let active = rustpbx::addons::cc::models::cc_skill_group::ActiveModel {
            skill_group_id: Set(id.to_string()),
            display_name: Set(display_name.map(str::to_string)),
            skills_required: Set(serde_json::json!(skills)),
            overflow_groups: Set(serde_json::json!([])),
            sla_target_secs: Set(30),
            max_wait_secs: Set(90),
            is_active: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        active.insert(db).await.unwrap();
    }

    /// Build a fake config path inside the temp dir.
    /// `skill_groups_combined_file` resolves to `<tempdir>/cc/skill_groups/skill_groups.generated.toml`
    /// when `config_path = "<tempdir>/config.toml"`.
    fn config_path_for(dir: &TempDir) -> String {
        dir.path().join("config.toml").to_string_lossy().to_string()
    }

    // ── export ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_export_creates_toml_file() {
        let db = setup_db().await;
        insert_skill_group(&db, "support-l1", Some("Support L1"), &["support"]).await;
        insert_skill_group(&db, "billing", None, &["billing", "finance"]).await;

        let state = CcAddonState::with_db(db);
        let tmp = TempDir::new().unwrap();
        let config_path = config_path_for(&tmp);

        let count = state
            .export_skill_groups_config(Some(&config_path))
            .await
            .expect("export should succeed");

        assert_eq!(count, 2, "should export 2 active skill groups");

        let combined = CcAddonState::skill_groups_combined_file(Some(&config_path)).unwrap();
        assert!(
            combined.exists(),
            "skill_groups.generated.toml should be created"
        );

        let content = std::fs::read_to_string(&combined).unwrap();
        assert!(
            content.contains("support-l1") && content.contains("billing"),
            "combined file should contain both groups"
        );
        // Should be an array-style file
        assert!(
            content.contains("[[skill_groups]]"),
            "combined file should use [[skill_groups]] array"
        );
    }

    #[tokio::test]
    async fn test_export_creates_backup_on_second_call() {
        let db = setup_db().await;
        insert_skill_group(&db, "support", None, &["support"]).await;

        let state = CcAddonState::with_db(db);
        let tmp = TempDir::new().unwrap();
        let config_path = config_path_for(&tmp);

        // First export
        state
            .export_skill_groups_config(Some(&config_path))
            .await
            .unwrap();

        let combined = CcAddonState::skill_groups_combined_file(Some(&config_path)).unwrap();
        let backup_file = combined.with_extension("toml.bak");

        assert!(
            !backup_file.exists(),
            ".bak should not exist after first export"
        );

        // Second export → should create .bak
        state
            .export_skill_groups_config(Some(&config_path))
            .await
            .unwrap();

        assert!(combined.exists(), "generated file should still exist");
        assert!(
            backup_file.exists(),
            ".bak should be created on second export"
        );
    }

    #[tokio::test]
    async fn test_export_inactive_groups_excluded() {
        let db = setup_db().await;
        insert_skill_group(&db, "active-group", None, &["support"]).await;

        // Insert an inactive skill group manually
        let now = chrono::Utc::now();
        let inactive = rustpbx::addons::cc::models::cc_skill_group::ActiveModel {
            skill_group_id: Set("inactive-group".to_string()),
            display_name: Set(None),
            skills_required: Set(serde_json::json!([])),
            overflow_groups: Set(serde_json::json!([])),
            sla_target_secs: Set(30),
            max_wait_secs: Set(90),
            is_active: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        inactive.insert(&db).await.unwrap();

        let state = CcAddonState::with_db(db);
        let tmp = TempDir::new().unwrap();
        let config_path = config_path_for(&tmp);

        let count = state
            .export_skill_groups_config(Some(&config_path))
            .await
            .unwrap();

        assert_eq!(count, 1, "only active groups should be exported");

        let combined = CcAddonState::skill_groups_combined_file(Some(&config_path)).unwrap();
        assert!(
            combined.exists(),
            "skill_groups.generated.toml should exist"
        );
        let content = std::fs::read_to_string(combined).unwrap();
        assert!(
            content.contains("active-group"),
            "active group should appear"
        );
        assert!(
            !content.contains("inactive-group"),
            "inactive group should not appear"
        );
    }

    // ── reload ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_reload_inserts_new_groups_from_toml() {
        let db = setup_db().await;

        let state = CcAddonState::with_db(db.clone());
        let tmp = TempDir::new().unwrap();
        let config_path = config_path_for(&tmp);

        // Create cc/skill_groups/ dir and write a .toml file manually
        let sg_dir = tmp.path().join("cc").join("skill_groups");
        std::fs::create_dir_all(&sg_dir).unwrap();
        let toml_content = r#"
[[skill_groups]]
skill_group_id = "sales"
display_name = "Sales Team"
skills_required = ["sales", "crm"]
overflow_groups = []
sla_target_secs = 20
max_wait_secs = 60
"#;
        std::fs::write(sg_dir.join("test.toml"), toml_content).unwrap();

        let loaded = state
            .reload_skill_groups_config(Some(&config_path))
            .await
            .expect("reload should succeed");

        assert_eq!(loaded, 1, "should load 1 group");

        // Verify the group is now in DB
        use sea_orm::EntityTrait;
        let groups = rustpbx::addons::cc::models::cc_skill_group::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].skill_group_id, "sales");
        assert!(groups[0].is_active);

        let skills: Vec<String> =
            serde_json::from_value(groups[0].skills_required.clone()).unwrap();
        assert!(skills.contains(&"sales".to_string()));
        assert!(skills.contains(&"crm".to_string()));
    }

    #[tokio::test]
    async fn test_reload_deactivates_missing_groups() {
        let db = setup_db().await;
        // Pre-insert two groups
        insert_skill_group(&db, "support", None, &["support"]).await;
        insert_skill_group(&db, "billing", None, &["billing"]).await;

        let state = CcAddonState::with_db(db.clone());
        let tmp = TempDir::new().unwrap();
        let config_path = config_path_for(&tmp);

        // TOML only contains "support", omits "billing"
        let sg_dir = tmp.path().join("cc").join("skill_groups");
        std::fs::create_dir_all(&sg_dir).unwrap();
        let toml_content = r#"
[[skill_groups]]
skill_group_id = "support"
skills_required = ["support"]
overflow_groups = []
sla_target_secs = 30
max_wait_secs = 90
"#;
        std::fs::write(sg_dir.join("test.toml"), toml_content).unwrap();

        state
            .reload_skill_groups_config(Some(&config_path))
            .await
            .unwrap();

        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let billing = rustpbx::addons::cc::models::cc_skill_group::Entity::find()
            .filter(rustpbx::addons::cc::models::cc_skill_group::Column::SkillGroupId.eq("billing"))
            .one(&db)
            .await
            .unwrap()
            .expect("billing group should still exist in DB");

        assert!(!billing.is_active, "billing group should be deactivated");

        let support = rustpbx::addons::cc::models::cc_skill_group::Entity::find()
            .filter(rustpbx::addons::cc::models::cc_skill_group::Column::SkillGroupId.eq("support"))
            .one(&db)
            .await
            .unwrap()
            .expect("support group should exist");

        assert!(support.is_active, "support group should remain active");
    }

    #[tokio::test]
    async fn test_reload_updates_existing_group() {
        let db = setup_db().await;
        insert_skill_group(&db, "support", Some("Old Name"), &["support"]).await;

        let state = CcAddonState::with_db(db.clone());
        let tmp = TempDir::new().unwrap();
        let config_path = config_path_for(&tmp);

        let sg_dir = tmp.path().join("cc").join("skill_groups");
        std::fs::create_dir_all(&sg_dir).unwrap();
        let toml_content = r#"
[[skill_groups]]
skill_group_id = "support"
display_name = "New Name"
skills_required = ["support", "billing"]
overflow_groups = []
sla_target_secs = 25
max_wait_secs = 80
"#;
        std::fs::write(sg_dir.join("test.toml"), toml_content).unwrap();

        state
            .reload_skill_groups_config(Some(&config_path))
            .await
            .unwrap();

        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let updated = rustpbx::addons::cc::models::cc_skill_group::Entity::find()
            .filter(rustpbx::addons::cc::models::cc_skill_group::Column::SkillGroupId.eq("support"))
            .one(&db)
            .await
            .unwrap()
            .expect("support should exist");

        assert_eq!(updated.display_name, Some("New Name".to_string()));
        assert_eq!(updated.sla_target_secs, 25);
        assert!(updated.is_active);

        let skills: Vec<String> = serde_json::from_value(updated.skills_required).unwrap();
        assert!(
            skills.contains(&"billing".to_string()),
            "billing skill should be added"
        );
    }

    #[tokio::test]
    async fn test_reload_fails_when_file_missing() {
        let db = setup_db().await;
        let state = CcAddonState::with_db(db);
        let tmp = TempDir::new().unwrap();
        let config_path = config_path_for(&tmp);

        let result = state.reload_skill_groups_config(Some(&config_path)).await;
        assert!(result.is_err(), "reload without file should return error");
    }

    #[tokio::test]
    async fn test_export_then_reload_roundtrip() {
        let db = setup_db().await;
        insert_skill_group(&db, "support", Some("Support"), &["support", "billing"]).await;
        insert_skill_group(&db, "sales", None, &["sales"]).await;

        let state = CcAddonState::with_db(db.clone());
        let tmp = TempDir::new().unwrap();
        let config_path = config_path_for(&tmp);

        // Export
        let exported = state
            .export_skill_groups_config(Some(&config_path))
            .await
            .unwrap();
        assert_eq!(exported, 2);

        // Wipe DB groups (deactivate both)
        {
            use sea_orm::{ActiveModelTrait, EntityTrait, Set};
            let all = rustpbx::addons::cc::models::cc_skill_group::Entity::find()
                .all(&db)
                .await
                .unwrap();
            let now = chrono::Utc::now();
            for m in all {
                let mut active: rustpbx::addons::cc::models::cc_skill_group::ActiveModel = m.into();
                active.is_active = Set(false);
                active.updated_at = Set(now);
                active.update(&db).await.unwrap();
            }
        }

        // Reload from the exported file
        let reloaded = state
            .reload_skill_groups_config(Some(&config_path))
            .await
            .unwrap();
        assert_eq!(reloaded, 2);

        // Both groups should be active again
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let active_groups = rustpbx::addons::cc::models::cc_skill_group::Entity::find()
            .filter(rustpbx::addons::cc::models::cc_skill_group::Column::IsActive.eq(true))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            active_groups.len(),
            2,
            "both groups should be re-activated after reload"
        );
    }

    // ── directory loading (load_skill_groups_from_dir_path) ───────────────────

    /// Write a file into `<tmp>/cc/skill_groups/` and return the directory path.
    fn write_sg_file(tmp: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let dir = tmp.path().join("cc").join("skill_groups");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_dir_load_single_entry_toml() {
        let tmp = TempDir::new().unwrap();
        let content = r#"
skill_group_id = "tier1"
display_name = "Tier 1 Support"
skills_required = ["support"]
"#;
        let dir = write_sg_file(&tmp, "tier1.generated.toml", content);

        let cache = rustpbx::addons::cc::CcAddonState::load_skill_groups_from_dir(Some(
            &config_path_for(&tmp),
        ))
        .await;

        assert_eq!(cache.groups.len(), 1, "should load one group");
        assert!(cache.groups.contains_key("tier1"));
        assert_eq!(cache.files.len(), 1);
        assert_eq!(cache.files[0].count, 1);
        let _ = dir; // keep alive
    }

    #[tokio::test]
    async fn test_dir_load_array_toml() {
        let tmp = TempDir::new().unwrap();
        let content = r#"
[[skill_groups]]
skill_group_id = "sales"
skills_required = ["sales"]

[[skill_groups]]
skill_group_id = "billing"
skills_required = ["billing"]
"#;
        let dir = write_sg_file(&tmp, "multi.generated.toml", content);

        let cache = rustpbx::addons::cc::CcAddonState::load_skill_groups_from_dir(Some(
            &config_path_for(&tmp),
        ))
        .await;

        assert_eq!(cache.groups.len(), 2);
        assert!(cache.groups.contains_key("sales"));
        assert!(cache.groups.contains_key("billing"));
        assert_eq!(cache.files[0].count, 2);
        let _ = dir;
    }

    #[tokio::test]
    async fn test_dir_load_mixed_directory() {
        let tmp = TempDir::new().unwrap();
        let single = r#"
skill_group_id = "tier1"
skills_required = ["support"]
"#;
        let array = r#"
[[skill_groups]]
skill_group_id = "sales"
skills_required = ["sales"]

[[skill_groups]]
skill_group_id = "billing"
skills_required = ["billing"]
"#;
        write_sg_file(&tmp, "tier1.generated.toml", single);
        write_sg_file(&tmp, "multi.generated.toml", array);

        let cache = rustpbx::addons::cc::CcAddonState::load_skill_groups_from_dir(Some(
            &config_path_for(&tmp),
        ))
        .await;

        assert_eq!(
            cache.groups.len(),
            3,
            "should load 3 total groups from 2 files"
        );
        assert_eq!(cache.files.len(), 2);
    }

    #[tokio::test]
    async fn test_dir_load_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("cc").join("skill_groups");
        std::fs::create_dir_all(&dir).unwrap();

        let cache = rustpbx::addons::cc::CcAddonState::load_skill_groups_from_dir(Some(
            &config_path_for(&tmp),
        ))
        .await;

        assert!(cache.groups.is_empty());
        assert!(cache.files.is_empty());
    }

    #[tokio::test]
    async fn test_dir_load_missing_directory() {
        let tmp = TempDir::new().unwrap();
        // Do NOT create the skill_groups dir.
        let cache = rustpbx::addons::cc::CcAddonState::load_skill_groups_from_dir(Some(
            &config_path_for(&tmp),
        ))
        .await;

        assert!(cache.groups.is_empty());
    }

    #[tokio::test]
    async fn test_dir_load_ignores_invalid_toml() {
        let tmp = TempDir::new().unwrap();
        let bad = "this is not valid toml !!!";
        let good = r#"
skill_group_id = "valid-group"
skills_required = ["support"]
"#;
        write_sg_file(&tmp, "bad.toml", bad);
        write_sg_file(&tmp, "good.generated.toml", good);

        let cache = rustpbx::addons::cc::CcAddonState::load_skill_groups_from_dir(Some(
            &config_path_for(&tmp),
        ))
        .await;

        // Only the good file contributes; the bad file is silently skipped.
        assert_eq!(cache.groups.len(), 1);
        assert!(cache.groups.contains_key("valid-group"));
    }

    #[tokio::test]
    async fn test_dir_load_non_toml_files_ignored() {
        let tmp = TempDir::new().unwrap();
        let content = r#"
skill_group_id = "tier1"
skills_required = ["support"]
"#;
        write_sg_file(&tmp, "tier1.generated.toml", content);
        write_sg_file(&tmp, "README.md", "# readme");
        write_sg_file(&tmp, "notes.txt", "some notes");

        let cache = rustpbx::addons::cc::CcAddonState::load_skill_groups_from_dir(Some(
            &config_path_for(&tmp),
        ))
        .await;

        assert_eq!(cache.groups.len(), 1, "non-toml files should be ignored");
        assert_eq!(
            cache.files.len(),
            1,
            "only the .toml file should be tracked"
        );
    }

    /// Verify that the sync loader produces the same result as the async loader.
    #[tokio::test]
    async fn test_sync_loader_matches_async() {
        let tmp = TempDir::new().unwrap();
        let content = r#"
[[skill_groups]]
skill_group_id = "alpha"
skills_required = ["a"]

[[skill_groups]]
skill_group_id = "beta"
skills_required = ["b"]
"#;
        write_sg_file(&tmp, "ab.generated.toml", content);

        let config_path = config_path_for(&tmp);

        let async_cache =
            rustpbx::addons::cc::CcAddonState::load_skill_groups_from_dir(Some(&config_path)).await;

        let cc_dir = tmp.path().to_path_buf();
        let sync_cache =
            rustpbx::addons::cc::CcAddonState::load_skill_groups_from_config_dir_sync(&cc_dir);

        assert_eq!(
            async_cache.groups.len(),
            sync_cache.groups.len(),
            "sync and async loaders should find the same number of groups"
        );
        for key in async_cache.groups.keys() {
            assert!(
                sync_cache.groups.contains_key(key),
                "key '{}' found in async cache but not sync cache",
                key
            );
        }
    }
