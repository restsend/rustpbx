//! Queue addon helpers that depend on DB models / export.
//!
//! File-format primitives (`QueueFileDocument`, `canonical_queue_key`,
//! `slugify_queue_name`, `load_queues_from_files`) live in
//! [`crate::proxy::queue_files`] so the proxy data plane does not import this
//! addon. Re-exported here for existing addon call sites.

use anyhow::{Context, Result};
use std::{cmp::Ordering, fs, path::Path};

use crate::{
    addons::queue::models as queue, config_store::GeneratedConfigStore,
    proxy::queue_files::QueueFileDocument, proxy::routing::RouteQueueConfig,
};

pub use crate::proxy::queue_files::{
    canonical_queue_key, load_queues_from_files, slugify_queue_name,
};

#[derive(Debug, Clone)]
pub struct QueueExportEntry {
    pub id: Option<i64>,
    pub name: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub queue: RouteQueueConfig,
}

impl QueueExportEntry {
    pub fn file_name(&self) -> String {
        let prefix = self
            .id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "local".to_string());
        let mut slug = slugify_queue_name(&self.name);
        if slug.is_empty() {
            slug = "queue".to_string();
        }
        format!("{}-{}.generated.toml", prefix, slug)
    }

    /// Get the key used for storing this entry.
    pub fn get_key(&self) -> String {
        queue_entry_key(self)
    }
}

pub fn convert_queue_model(model: queue::Model) -> Result<QueueExportEntry> {
    let queue_config: RouteQueueConfig = serde_json::from_value(model.spec.clone())
        .with_context(|| format!("queue '{}' has invalid spec payload", model.name))?;
    let tags = metadata_tags(model.metadata.as_ref());
    Ok(QueueExportEntry {
        id: Some(model.id),
        name: model.name,
        description: model.description,
        tags,
        queue: queue_config,
    })
}

fn metadata_tags(metadata: Option<&serde_json::Value>) -> Vec<String> {
    let Some(value) = metadata else {
        return Vec::new();
    };
    if let Some(tags_value) = value.get("tags")
        && let Ok(tags) = serde_json::from_value::<Vec<String>>(tags_value.clone())
    {
        return normalize_tags(tags);
    }
    Vec::new()
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut results: Vec<String> = Vec::new();
    for tag in tags {
        let cleaned = tag.trim();
        if cleaned.is_empty() {
            continue;
        }
        if results
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(cleaned))
        {
            continue;
        }
        results.push(cleaned.to_string());
    }
    results
}

pub fn queue_entry_key(entry: &QueueExportEntry) -> String {
    if let Some(id) = entry.id {
        format!("db-{}", id)
    } else {
        canonical_queue_key(&entry.name)
            .unwrap_or_else(|| format!("local-{}", slugify_queue_name(&entry.name)))
    }
}

pub fn queue_export_entry_cmp(a: &QueueExportEntry, b: &QueueExportEntry) -> Ordering {
    match (a.id, b.id) {
        (Some(id_a), Some(id_b)) => id_a.cmp(&id_b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => {
            let left = canonical_queue_key(&a.name).unwrap_or_else(|| a.name.clone());
            let right = canonical_queue_key(&b.name).unwrap_or_else(|| b.name.clone());
            left.cmp(&right)
        }
    }
}

pub fn write_queue_file(path: &Path, entry: &QueueExportEntry) -> Result<()> {
    ensure_parent_dir(path)?;
    let toml_doc = serialize_queue_entry(entry)?;
    fs::write(path, toml_doc)
        .with_context(|| format!("failed to write queue file {}", path.display()))?;
    Ok(())
}

pub fn serialize_queue_entry(entry: &QueueExportEntry) -> Result<String> {
    let doc = QueueFileDocument {
        id: entry.id,
        name: entry.name.clone(),
        description: entry.description.clone(),
        tags: entry.tags.clone(),
        queue: entry.queue.clone(),
    };
    toml::to_string_pretty(&doc)
        .with_context(|| format!("failed to serialize queue toml for {}", entry.name))
}

pub fn cleanup_queue_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)
        .with_context(|| format!("failed to read queue directory {}", dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to walk queue directory {}", dir.display()))?;
        let path = entry.path();
        if path.is_file() {
            let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
            if matches!(extension, "yml" | "yaml" | "toml") {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove queue file {}", path.display()))?;
            }
        }
    }
    Ok(())
}

/// Clean up queue entries in the config store for a given set of names.
pub async fn cleanup_queue_store(
    store: &GeneratedConfigStore,
    keep_names: &[String],
) -> Result<()> {
    let names = store.list_names("queue").await?;
    for name in names {
        if !keep_names.contains(&name) {
            store.delete("queue", &name).await?;
        }
    }
    Ok(())
}

/// Write a queue entry to the config store.
pub async fn write_queue_entry_to_store(
    store: &GeneratedConfigStore,
    entry: &QueueExportEntry,
) -> Result<String> {
    let filename = format!("{}.toml", entry.get_key());
    let toml_doc = serialize_queue_entry(entry)?;
    store.write("queue", &filename, &toml_doc).await?;
    Ok(filename)
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}
