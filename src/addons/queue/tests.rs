#[cfg(test)]
mod tests {
    use crate::addons::queue::models as queue;
    use crate::addons::queue::services::exporter::QueueExporter;
    use crate::config::ProxyConfig;
    use crate::proxy::routing::{RouteQueueConfig, RouteQueueHoldConfig};
    use sea_orm::{ActiveValue::Set, ConnectionTrait, Database, EntityTrait, Schema};
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_export_queues_hold_music_priority() {
        // Setup in-memory DB
        let db = Database::connect("sqlite::memory:").await.unwrap();

        // Create table
        let builder = db.get_database_backend();
        let schema = Schema::new(builder);
        let stmt = schema.create_table_from_entity(queue::Entity);
        db.execute_raw(builder.build(&stmt)).await.unwrap();

        // Insert data
        let model1 = queue::ActiveModel {
            name: Set("sales".to_string()),
            description: Set(Some("Sales Queue".to_string())),
            spec: Set(json!(RouteQueueConfig::default())),
            is_active: Set(true),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            last_exported_at: Set(None),
            ..Default::default()
        };
        queue::Entity::insert(model1).exec(&db).await.unwrap();

        let mut explicit_audio = RouteQueueConfig::default();
        explicit_audio.hold = Some(RouteQueueHoldConfig {
            audio_file: Some(crate::call::DEFAULT_QUEUE_HOLD_AUDIO.to_string()),
            loop_playback: false,
        });
        let model2 = queue::ActiveModel {
            name: Set("support".to_string()),
            description: Set(Some("Support Queue".to_string())),
            spec: Set(json!(explicit_audio)),
            is_active: Set(true),
            created_at: Set(chrono::Utc::now()),
            updated_at: Set(chrono::Utc::now()),
            last_exported_at: Set(None),
            ..Default::default()
        };
        queue::Entity::insert(model2).exec(&db).await.unwrap();

        // Setup temp dir for config
        let temp_dir = tempdir().unwrap();
        let mut config = ProxyConfig::default();
        config.generated_dir = temp_dir.path().to_string_lossy().to_string();

        let exporter = QueueExporter::new(db);
        let result = exporter.export_all(&config).await.unwrap();

        assert_eq!(result.len(), 1);
        let file_path = &result[0];
        assert!(file_path.ends_with("queues.generated.toml"));

        let content = fs::read_to_string(file_path).unwrap();
        println!("Generated TOML:\n{}", content);

        // Verify content
        let parsed: std::collections::HashMap<String, RouteQueueConfig> =
            toml::from_str(&content).unwrap();
        assert_eq!(parsed.len(), 2);

        let names: Vec<String> = parsed.values().filter_map(|v| v.name.clone()).collect();
        assert!(names.contains(&"sales".to_string()));
        assert!(names.contains(&"support".to_string()));

        for key in parsed.keys() {
            assert!(key.starts_with("db-"));
        }
        for queue in parsed.values() {
            assert_eq!(
                queue.hold.as_ref().unwrap().audio_file.as_deref(),
                Some(crate::call::DEFAULT_QUEUE_HOLD_AUDIO),
                "without global music, preserve the original queue audio"
            );
        }

        config.queue_hold_music = Some("sounds/wait.wav".into());
        exporter.export_all(&config).await.unwrap();
        let parsed: std::collections::HashMap<String, RouteQueueConfig> =
            toml::from_str(&fs::read_to_string(file_path).unwrap()).unwrap();
        for queue in parsed.values() {
            let hold = queue.hold.as_ref().unwrap();
            if queue.name.as_deref() == Some("sales") {
                assert_eq!(hold.audio_file.as_deref(), Some("sounds/wait.wav"));
            } else {
                assert_eq!(
                    hold.audio_file.as_deref(),
                    Some(crate::call::DEFAULT_QUEUE_HOLD_AUDIO)
                );
                assert!(
                    !hold.loop_playback,
                    "explicit queue options must survive export"
                );
            }
        }
    }
}
