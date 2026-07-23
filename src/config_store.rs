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

    /// Parse a virtual config-store URI of the form `db://<category>/<name>`.
    /// Returns `Some((category, name))` when the prefix matches and both
    /// segments are non-empty, or `None` otherwise.
    pub fn parse_db_uri(uri: &str) -> Option<(&str, &str)> {
        let rest = uri.strip_prefix("db://")?;
        let idx = rest.find('/')?;
        let category = &rest[..idx];
        let name = &rest[idx + 1..];
        if category.is_empty() || name.is_empty() {
            return None;
        }
        Some((category, name))
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

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;

    // ── parse_db_uri ──────────────────────────────────────────────────────────

    #[test]
    fn parse_db_uri_valid_ivr() {
        let r = GeneratedConfigStore::parse_db_uri("db://ivr/main.generated.toml");
        assert_eq!(r, Some(("ivr", "main.generated.toml")));
    }

    #[test]
    fn parse_db_uri_valid_other_categories() {
        assert_eq!(
            GeneratedConfigStore::parse_db_uri("db://routes/routes.generated.toml"),
            Some(("routes", "routes.generated.toml"))
        );
        assert_eq!(
            GeneratedConfigStore::parse_db_uri("db://queue/my_queue.toml"),
            Some(("queue", "my_queue.toml"))
        );
        assert_eq!(
            GeneratedConfigStore::parse_db_uri("db://trunks/trunks.generated.toml"),
            Some(("trunks", "trunks.generated.toml"))
        );
        assert_eq!(
            GeneratedConfigStore::parse_db_uri("db://acl/acl.generated.toml"),
            Some(("acl", "acl.generated.toml"))
        );
    }

    #[test]
    fn parse_db_uri_non_db_paths() {
        // Absolute filesystem path
        assert_eq!(GeneratedConfigStore::parse_db_uri("/etc/ivr/main.toml"), None);
        // Relative filesystem path
        assert_eq!(GeneratedConfigStore::parse_db_uri("config/ivr/main.toml"), None);
        // Random protocol
        assert_eq!(GeneratedConfigStore::parse_db_uri("http://example.com/x"), None);
        assert_eq!(GeneratedConfigStore::parse_db_uri("file:///tmp/x.toml"), None);
        // Empty
        assert_eq!(GeneratedConfigStore::parse_db_uri(""), None);
    }

    #[test]
    fn parse_db_uri_edge_cases() {
        // Just the scheme, no category/name
        assert_eq!(GeneratedConfigStore::parse_db_uri("db://"), None);
        // No name segment
        assert_eq!(GeneratedConfigStore::parse_db_uri("db://ivr"), None);
        assert_eq!(GeneratedConfigStore::parse_db_uri("db://ivr/"), None);
        // Empty category
        assert_eq!(GeneratedConfigStore::parse_db_uri("db:///x.toml"), None);
        // Extra slashes (name is everything after first slash)
        assert_eq!(
            GeneratedConfigStore::parse_db_uri("db://ivr/sub/dir/file.toml"),
            Some(("ivr", "sub/dir/file.toml"))
        );
    }

    // ── DB round-trip ─────────────────────────────────────────────────────────

    async fn setup_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        // Run the core migrator to create config_entries table
        let schema = sea_orm::Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(config_entry::Entity);
        // Build the SQL for the current backend
        use sea_orm::{ConnectionTrait, sea_query::SqliteQueryBuilder};
        let sql = stmt.to_string(SqliteQueryBuilder);
        db.execute_unprepared(&sql).await.unwrap();
        db
    }

    #[tokio::test]
    async fn db_write_read_roundtrip() {
        let db = setup_db().await;
        let store = GeneratedConfigStore::Database { db: db.clone() };

        store
            .write("ivr", "test.generated.toml", "some_content")
            .await
            .unwrap();

        let content = store.read("ivr", "test.generated.toml").await.unwrap();
        assert_eq!(content, Some("some_content".to_string()));
    }

    #[tokio::test]
    async fn db_exists() {
        let db = setup_db().await;
        let store = GeneratedConfigStore::Database { db: db.clone() };

        store
            .write("ivr", "exists_test.toml", "x")
            .await
            .unwrap();

        assert!(store.exists("ivr", "exists_test.toml").await.unwrap());
        assert!(!store.exists("ivr", "nonexistent.toml").await.unwrap());
    }

    #[tokio::test]
    async fn db_list_names() {
        let db = setup_db().await;
        let store = GeneratedConfigStore::Database { db: db.clone() };

        store.write("ivr", "a.toml", "1").await.unwrap();
        store.write("ivr", "b.toml", "2").await.unwrap();
        store.write("queue", "q.toml", "3").await.unwrap();

        let ivr_names = store.list_names("ivr").await.unwrap();
        assert_eq!(ivr_names, vec!["a.toml", "b.toml"]);

        let queue_names = store.list_names("queue").await.unwrap();
        assert_eq!(queue_names, vec!["q.toml"]);
    }

    #[tokio::test]
    async fn db_delete() {
        let db = setup_db().await;
        let store = GeneratedConfigStore::Database { db: db.clone() };

        store.write("ivr", "del_test.toml", "x").await.unwrap();
        assert!(store.exists("ivr", "del_test.toml").await.unwrap());

        store.delete("ivr", "del_test.toml").await.unwrap();
        assert!(!store.exists("ivr", "del_test.toml").await.unwrap());
    }

    #[tokio::test]
    async fn db_cleanup_category_preserves_non_generated() {
        let db = setup_db().await;
        let store = GeneratedConfigStore::Database { db: db.clone() };

        store.write("ivr", "a.generated.toml", "x").await.unwrap();
        store.write("ivr", "b.generated.toml", "y").await.unwrap();
        store.write("ivr", "handwritten.toml", "z").await.unwrap();

        store.cleanup_category("ivr").await.unwrap();

        // generated files should be removed
        assert!(!store.exists("ivr", "a.generated.toml").await.unwrap());
        assert!(!store.exists("ivr", "b.generated.toml").await.unwrap());
        // handwritten should survive
        assert!(store.exists("ivr", "handwritten.toml").await.unwrap());
    }

    #[tokio::test]
    async fn db_upsert_overwrites_content() {
        let db = setup_db().await;
        let store = GeneratedConfigStore::Database { db: db.clone() };

        store
            .write("ivr", "upsert.toml", "first")
            .await
            .unwrap();
        store
            .write("ivr", "upsert.toml", "second")
            .await
            .unwrap();

        let content = store.read("ivr", "upsert.toml").await.unwrap();
        assert_eq!(content, Some("second".to_string()));

        // Only one row should exist
        use crate::models::config_entry;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let count = config_entry::Entity::find()
            .filter(config_entry::Column::Category.eq("ivr"))
            .filter(config_entry::Column::EntryName.eq("upsert.toml"))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn db_read_nonexistent_returns_none() {
        let db = setup_db().await;
        let store = GeneratedConfigStore::Database { db };
        let content = store.read("ivr", "nonexistent.toml").await.unwrap();
        assert_eq!(content, None);
    }
}
