use anyhow::{Context, Result};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use std::{collections::HashMap, fs, path::Path};

use crate::{
    addons::queue::models as queue, addons::queue::services::utils as queue_utils,
    call::DEFAULT_QUEUE_HOLD_AUDIO,
    config::ProxyConfig,
    proxy::routing::{RouteQueueConfig, RouteQueueHoldConfig},
};

pub struct QueueExporter {
    db: DatabaseConnection,
}

impl QueueExporter {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn export_queue(&self, _id: i64, config: &ProxyConfig) -> Result<Option<String>> {
        let paths = self.export_all(config).await?;
        Ok(paths.first().cloned())
    }

    pub async fn export_all(&self, config: &ProxyConfig) -> Result<Vec<String>> {
        let dir = config.generated_queue_dir();
        ensure_queue_dir(&dir)?;

        let models = queue::Entity::find()
            .filter(queue::Column::IsActive.eq(true))
            .order_by_asc(queue::Column::Name)
            .all(&self.db)
            .await?;

        let mut queues_map = HashMap::new();
        for model in models {
            let entry = queue_utils::convert_queue_model(model)?;
            let key = queue_utils::queue_entry_key(&entry);
            let mut queue_config = entry.queue;
            queue_config.name = Some(entry.name);
            normalize_queue_audio_paths(&mut queue_config);
            queues_map.insert(key, queue_config);
        }

        let file_path = dir.join("queues.generated.toml");
        let toml_str =
            toml::to_string_pretty(&queues_map).context("failed to serialize queues to toml")?;

        fs::write(&file_path, toml_str)
            .with_context(|| format!("failed to write queues file {}", file_path.display()))?;

        Ok(vec![file_path.display().to_string()])
    }

    pub async fn remove_entry_file(
        &self,
        _entry: &queue_utils::QueueExportEntry,
        config: &ProxyConfig,
    ) -> Result<Option<String>> {
        let paths = self.export_all(config).await?;
        Ok(paths.first().cloned())
    }
}

fn ensure_queue_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create queue dir {}", dir.display()))?;
    }
    Ok(())
}

fn normalize_queue_audio_paths(queue_config: &mut RouteQueueConfig) {
    match queue_config.hold.as_mut() {
        Some(hold) => normalize_optional_audio_path(&mut hold.audio_file),
        None => {
            queue_config.hold = Some(RouteQueueHoldConfig {
                audio_file: Some(normalize_packaged_audio_path(DEFAULT_QUEUE_HOLD_AUDIO)),
                loop_playback: true,
            });
        }
    }

    if let Some(fallback) = queue_config.fallback.as_mut() {
        normalize_optional_audio_path(&mut fallback.failure_prompt);
    }

    if let Some(prompts) = queue_config.voice_prompts.as_mut() {
        normalize_optional_audio_path(&mut prompts.transfer_prompt);
        normalize_optional_audio_path(&mut prompts.busy_prompt);
        normalize_optional_audio_path(&mut prompts.off_hours_prompt);
        normalize_optional_audio_path(&mut prompts.no_answer_prompt);
    }
}

fn normalize_optional_audio_path(path: &mut Option<String>) {
    if let Some(value) = path.as_mut() {
        *value = normalize_packaged_audio_path(value);
    }
}

fn normalize_packaged_audio_path(path: &str) -> String {
    path.strip_prefix("config/sounds/")
        .map(|file| format!("sounds/{file}"))
        .unwrap_or_else(|| path.to_string())
}
