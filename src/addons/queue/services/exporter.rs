use anyhow::{Context, Result};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use std::{collections::HashMap, fs, path::Path};

use crate::{config::ProxyConfig, addons::queue::models as queue, addons::queue::services::utils as queue_utils};

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
            queues_map.insert(key, queue_config);
        }

        let file_path = dir.join("queues.generated.toml");
        let toml_str = toml::to_string_pretty(&queues_map)
            .context("failed to serialize queues to toml")?;
        
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
