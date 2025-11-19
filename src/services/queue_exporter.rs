use anyhow::{Context, Result};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use std::{fs, path::Path};

use crate::{config::ProxyConfig, models::queue, services::queue_utils};

pub struct QueueExporter {
    db: DatabaseConnection,
}

impl QueueExporter {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn export_queue(&self, id: i64, config: &ProxyConfig) -> Result<Option<String>> {
        let model = match queue::Entity::find_by_id(id).one(&self.db).await? {
            Some(model) => model,
            None => return Ok(None),
        };

        let entry = queue_utils::convert_queue_model(model)?;
        let dir = config.generated_queue_dir();
        ensure_queue_dir(&dir)?;
        let file_path = dir.join(entry.file_name());
        queue_utils::write_queue_file(file_path.as_path(), &entry)?;
        Ok(Some(file_path.display().to_string()))
    }

    pub async fn export_all(&self, config: &ProxyConfig) -> Result<Vec<String>> {
        let dir = config.generated_queue_dir();
        ensure_queue_dir(&dir)?;
        queue_utils::cleanup_queue_dir(&dir)?;

        let models = queue::Entity::find()
            .filter(queue::Column::IsActive.eq(true))
            .order_by_asc(queue::Column::Name)
            .all(&self.db)
            .await?;

        let mut entries = Vec::new();
        for model in models {
            let entry = queue_utils::convert_queue_model(model)?;
            entries.push(entry);
        }

        let mut written = Vec::new();
        for entry in entries {
            let file_path = dir.join(entry.file_name());
            queue_utils::write_queue_file(file_path.as_path(), &entry)?;
            written.push(file_path.display().to_string());
        }

        Ok(written)
    }

    pub fn remove_entry_file(
        &self,
        entry: &queue_utils::QueueExportEntry,
        config: &ProxyConfig,
    ) -> Result<Option<String>> {
        let file_path = config.generated_queue_dir().join(entry.file_name());
        if file_path.exists() {
            fs::remove_file(&file_path)
                .with_context(|| format!("failed to remove queue file {}", file_path.display()))?;
            return Ok(Some(file_path.display().to_string()));
        }
        Ok(None)
    }
}

fn ensure_queue_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create queue dir {}", dir.display()))?;
    }
    Ok(())
}
