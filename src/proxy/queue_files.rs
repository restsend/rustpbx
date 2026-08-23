//! Queue TOML file format helpers used by the proxy data plane.
//!
//! Lives in core (not the queue addon) because [`ProxyDataContext`] and the
//! console catalog load queue definitions as part of routing config. The queue
//! addon re-exports / builds on these helpers for DB export.

use anyhow::{Context, Result, anyhow};
use glob::glob;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::proxy::routing::RouteQueueConfig;

/// On-disk / config-store document for a single queue definition.
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

/// Canonical lookup key for a queue name (`trim` + lowercase). Empty → `None`.
pub fn canonical_queue_key(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

/// Filesystem-safe slug derived from a display name.
pub fn slugify_queue_name(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            last_dash = false;
        } else if (lower.is_ascii_whitespace() || matches!(lower, '-' | '_' | '.' | '/'))
            && !slug.is_empty()
            && !last_dash
        {
            slug.push('-');
            last_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Load queue definitions from glob include patterns (`[proxy].queues_files`).
pub async fn load_queues_from_files(
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
            let contents = tokio::fs::read_to_string(&path)
                .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_queue_name_strips_whitespace() {
        assert_eq!(slugify_queue_name("  Sales Support  "), "sales-support");
        assert_eq!(slugify_queue_name("UPPER_case"), "upper-case");
        assert_eq!(slugify_queue_name("..special??"), "special");
    }

    #[test]
    fn canonical_queue_key_trims_and_lowercases() {
        assert_eq!(canonical_queue_key("  Sales  ").as_deref(), Some("sales"));
        assert!(canonical_queue_key("   ").is_none());
    }
}
