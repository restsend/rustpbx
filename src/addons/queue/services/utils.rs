use anyhow::{Context, Result, anyhow};
use glob::glob;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashMap, fs, path::Path};
use tracing::{info, warn};

use crate::{addons::queue::models as queue, proxy::routing::RouteQueueConfig};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QueueFileDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub queue: RouteQueueConfig,
}

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
    if let Some(tags_value) = value.get("tags") {
        if let Ok(tags) = serde_json::from_value::<Vec<String>>(tags_value.clone()) {
            return normalize_tags(tags);
        }
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
    let doc = QueueFileDocument {
        id: entry.id,
        name: entry.name.clone(),
        description: entry.description.clone(),
        tags: entry.tags.clone(),
        queue: entry.queue.clone(),
    };
    let toml_doc = toml::to_string_pretty(&doc)
        .with_context(|| format!("failed to serialize queue toml for {}", entry.name))?;
    fs::write(path, toml_doc)
        .with_context(|| format!("failed to write queue file {}", path.display()))?;
    Ok(())
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

pub fn canonical_queue_key(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

pub fn slugify_queue_name(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            last_dash = false;
        } else if lower.is_ascii_whitespace() || matches!(lower, '-' | '_' | '.' | '/') {
            if !slug.is_empty() && !last_dash {
                slug.push('-');
                last_dash = true;
            }
        }
    }
    slug.trim_matches('-').to_string()
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    Ok(())
}

pub fn load_queues_from_files(
    patterns: &[String],
) -> Result<(HashMap<String, RouteQueueConfig>, Vec<String>)> {
    let mut queues: HashMap<String, RouteQueueConfig> = HashMap::new();
    let mut files: Vec<String> = Vec::new();
    for pattern in patterns {
        if pattern.trim().is_empty() {
            continue;
        }
        let entries = glob(pattern)
            .map_err(|e| anyhow!("invalid queue include pattern '{}': {}", pattern, e))?;
        for entry in entries {
            let path =
                entry.map_err(|e| anyhow!("failed to read queue include glob entry: {}", e))?;
            let path_display = path.display().to_string();
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read queue include file {}", path_display))?;
            let doc: QueueFileDocument = toml::from_str(&contents)
                .with_context(|| format!("failed to parse queue include file {}", path_display))?;
            let Some(key) = canonical_queue_key(&doc.name) else {
                return Err(anyhow!(
                    "queue include file {} is missing a valid name",
                    path_display
                ));
            };
            if !files.contains(&path_display) {
                files.push(path_display.clone());
            }
            if queues.contains_key(&key) {
                warn!(queue = %doc.name, file = %path_display, "queue definition overridden by a later include");
            }
            info!(queue = %doc.name, file = %path_display, "loaded queue from include file");
            queues.insert(key, doc.queue.clone());
        }
    }
    Ok((queues, files))
}
