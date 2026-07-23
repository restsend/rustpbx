use anyhow::{Context, Result, anyhow};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::models::config_entry;

#[derive(Debug, Clone)]
pub enum GeneratedConfigStore {
    FileSystem { root: PathBuf },
    Database { db: DatabaseConnection },
}

impl GeneratedConfigStore {
    pub fn fs(root: PathBuf) -> Self {
        Self::FileSystem { root }
    }

    pub fn db(db: DatabaseConnection) -> Self {
        Self::Database { db }
    }

    pub fn from_config(config: &Config, db: &DatabaseConnection) -> Self {
        if config.proxy.use_db_config() {
            Self::Database { db: db.clone() }
        } else {
            Self::FileSystem {
                root: config.config_dir(),
            }
        }
    }

    pub fn is_db(&self) -> bool {
        matches!(self, Self::Database { .. })
    }

    pub async fn read(&self, category: &str, name: &str) -> Result<Option<String>> {
        match self {
            Self::FileSystem { root } => {
                let path = root.join(category).join(name);
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => Ok(Some(content)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e)
                        .with_context(|| format!("failed to read config file {}", path.display())),
                }
            }
            Self::Database { db } => {
                let result = config_entry::Entity::find()
                    .filter(config_entry::Column::Category.eq(category))
                    .filter(config_entry::Column::EntryName.eq(name))
                    .one(db)
                    .await
                    .map_err(|e| anyhow!("db config read error: {e}"))?;
                Ok(result.map(|m| m.content))
            }
        }
    }

    pub async fn write(&self, category: &str, name: &str, content: &str) -> Result<()> {
        match self {
            Self::FileSystem { root } => {
                let path = root.join(category).join(name);
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await.with_context(|| {
                        format!("failed to create directory {}", parent.display())
                    })?;
                }
                tokio::fs::write(&path, content)
                    .await
                    .with_context(|| format!("failed to write config file {}", path.display()))?;
                Ok(())
            }
            Self::Database { db } => self.write_inner(db, category, name, content).await,
        }
    }

    pub async fn delete(&self, category: &str, name: &str) -> Result<()> {
        match self {
            Self::FileSystem { root } => {
                let path = root.join(category).join(name);
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e).with_context(|| {
                        format!("failed to delete config file {}", path.display())
                    }),
                }
            }
            Self::Database { db } => {
                config_entry::Entity::delete_many()
                    .filter(config_entry::Column::Category.eq(category))
                    .filter(config_entry::Column::EntryName.eq(name))
                    .exec(db)
                    .await
                    .map_err(|e| anyhow!("db config delete error: {e}"))?;
                Ok(())
            }
        }
    }

    pub async fn list_names(&self, category: &str) -> Result<Vec<String>> {
        match self {
            Self::FileSystem { root } => {
                let dir = root.join(category);
                let mut entries = match tokio::fs::read_dir(&dir).await {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("failed to read directory {}", dir.display()));
                    }
                };
                let mut names = Vec::new();
                while let Some(entry) = entries.next_entry().await? {
                    if entry.file_type().await?.is_file() {
                        if let Some(name) = entry.file_name().to_str() {
                            names.push(name.to_string());
                        }
                    }
                }
                names.sort();
                Ok(names)
            }
            Self::Database { db } => {
                let results = config_entry::Entity::find()
                    .filter(config_entry::Column::Category.eq(category))
                    .select_only()
                    .column(config_entry::Column::EntryName)
                    .order_by_asc(config_entry::Column::EntryName)
                    .into_tuple::<(String,)>()
                    .all(db)
                    .await
                    .map_err(|e| anyhow!("db config list error: {e}"))?;
                let names: Vec<String> = results.into_iter().map(|(n,)| n).collect();
                Ok(names)
            }
        }
    }

    pub async fn exists(&self, category: &str, name: &str) -> Result<bool> {
        match self {
            Self::FileSystem { root } => Ok(root.join(category).join(name).exists()),
            Self::Database { db } => {
                let count = config_entry::Entity::find()
                    .filter(config_entry::Column::Category.eq(category))
                    .filter(config_entry::Column::EntryName.eq(name))
                    .count(db)
                    .await
                    .map_err(|e| anyhow!("db config count error: {e}"))?;
                Ok(count > 0)
            }
        }
    }

    pub async fn cleanup_category(&self, category: &str) -> Result<()> {
        match self {
            Self::FileSystem { root } => {
                let dir = root.join(category);
                if !dir.exists() {
                    return Ok(());
                }
                let mut entries = tokio::fs::read_dir(&dir)
                    .await
                    .with_context(|| format!("failed to read directory {}", dir.display()))?;
                while let Some(entry) = entries.next_entry().await? {
                    let path = entry.path();
                    if path.is_file() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if name.ends_with(".generated.toml") {
                                let _ = tokio::fs::remove_file(&path).await;
                            }
                        }
                    }
                }
                Ok(())
            }
            Self::Database { db } => {
                config_entry::Entity::delete_many()
                    .filter(config_entry::Column::Category.eq(category))
                    .filter(config_entry::Column::EntryName.like("%.generated.toml"))
                    .exec(db)
                    .await
                    .map_err(|e| anyhow!("db config cleanup error: {e}"))?;
                Ok(())
            }
        }
    }

    async fn write_inner(
        &self,
        db: &DatabaseConnection,
        category: &str,
        name: &str,
        content: &str,
    ) -> Result<()> {
        let existing = config_entry::Entity::find()
            .filter(config_entry::Column::Category.eq(category))
            .filter(config_entry::Column::EntryName.eq(name))
            .one(db)
            .await
            .map_err(|e| anyhow!("db config check error: {e}"))?;

        let now = chrono::Utc::now();
        if let Some(record) = existing {
            let mut active: config_entry::ActiveModel = record.into();
            active.set(config_entry::Column::Content, content.into());
            active.set(config_entry::Column::UpdatedAt, now.into());
            active
                .update(db)
                .await
                .map_err(|e| anyhow!("db config update error: {e}"))?;
        } else {
            config_entry::ActiveModel {
                category: Set(category.to_string()),
                entry_name: Set(name.to_string()),
                content: Set(content.to_string()),
                is_generated: Set(true),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            }
            .insert(db)
            .await
            .map_err(|e| anyhow!("db config insert error: {e}"))?;
        }
        Ok(())
    }

    /// Get the root path (only meaningful for FileSystem mode)
    pub fn root_path(&self) -> Option<&Path> {
        match self {
            Self::FileSystem { root } => Some(root),
            Self::Database { .. } => None,
        }
    }
}
