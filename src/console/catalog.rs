use crate::addons::queue::services::utils::{
    QueueFileDocument, canonical_queue_key, slugify_queue_name,
};
use crate::call::app::ivr_config::IvrFileConfig;
use crate::config::ProxyConfig;
use crate::proxy::routing::RouteQueueConfig;
use glob::glob;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct IvrCatalogEntry {
    pub name: String,
    pub description: Option<String>,
    pub ivr_mode: String,
    pub file_path: String,
    pub generated: bool,
}

#[derive(Debug, Clone)]
pub struct QueueCatalogEntry {
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    pub file_path: String,
    pub generated: bool,
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn scan_ivr_catalog(ivr_dir: &Path, extra_patterns: &[String]) -> Vec<IvrCatalogEntry> {
    let mut seen: HashMap<String, IvrCatalogEntry> = HashMap::new();

    if ivr_dir.exists() {
        if let Ok(entries) = fs::read_dir(ivr_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let file_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !file_name.ends_with(".toml") {
                    continue;
                }

                let generated = file_name.ends_with(".generated.toml");

                let project_name = if generated {
                    file_name
                        .strip_suffix(".generated.toml")
                        .unwrap_or(&file_name)
                } else {
                    file_name.strip_suffix(".toml").unwrap_or(&file_name)
                };

                let content = match fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("failed to read IVR file {}: {}", path.display(), e);
                        continue;
                    }
                };

                let file_config: IvrFileConfig = match toml::from_str(&content) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("failed to parse IVR file {}: {}", path.display(), e);
                        continue;
                    }
                };

                let ivr = file_config.ivr;
                let ivr_name = if ivr.name.trim().is_empty() {
                    project_name.to_string()
                } else {
                    ivr.name.clone()
                };
                let ivr_mode = ivr.ivr_mode.clone().unwrap_or_else(|| "tree".to_string());
                let lookup_key = sanitize_filename(&ivr_name);

                let existing_is_generated = seen.get(&lookup_key).map(|e| e.generated);
                if existing_is_generated == Some(true) && !generated {
                    let old = seen.remove(&lookup_key).unwrap();
                    warn!(
                        "IVR '{}' has both hand-written and generated files; preferring hand-written ({})",
                        ivr_name, old.file_path
                    );
                }
                if seen.contains_key(&lookup_key) {
                    continue;
                }

                seen.insert(
                    lookup_key,
                    IvrCatalogEntry {
                        name: ivr_name,
                        description: ivr.description,
                        ivr_mode,
                        file_path: path.display().to_string(),
                        generated,
                    },
                );
            }
        }
    }

    for pattern in extra_patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entries = match glob(trimmed) {
            Ok(e) => e,
            Err(e) => {
                warn!("invalid IVR include pattern '{}': {}", trimmed, e);
                continue;
            }
        };
        for glob_entry in entries {
            let path = match glob_entry {
                Ok(p) => p,
                Err(e) => {
                    warn!("failed to read IVR include glob entry: {}", e);
                    continue;
                }
            };
            if !path.is_file() {
                continue;
            }
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let generated = file_name.ends_with(".generated.toml");

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("failed to read IVR include file {}: {}", path.display(), e);
                    continue;
                }
            };

            let file_config: IvrFileConfig = match toml::from_str(&content) {
                Ok(c) => c,
                Err(e) => {
                    warn!("failed to parse IVR include file {}: {}", path.display(), e);
                    continue;
                }
            };

            let ivr = file_config.ivr;
            let project_name = file_name
                .strip_suffix(".generated.toml")
                .or_else(|| file_name.strip_suffix(".toml"))
                .unwrap_or(&file_name);
            let ivr_name = if ivr.name.trim().is_empty() {
                project_name.to_string()
            } else {
                ivr.name.clone()
            };
            let ivr_mode = ivr.ivr_mode.clone().unwrap_or_else(|| "tree".to_string());
            let lookup_key = sanitize_filename(&ivr_name);

            let existing_is_generated = seen.get(&lookup_key).map(|e| e.generated);
            if existing_is_generated == Some(true) && !generated {
                seen.remove(&lookup_key);
            }
            if seen.contains_key(&lookup_key) {
                continue;
            }

            seen.insert(
                lookup_key,
                IvrCatalogEntry {
                    name: ivr_name,
                    description: ivr.description,
                    ivr_mode,
                    file_path: path.display().to_string(),
                    generated,
                },
            );
        }
    }

    let mut result: Vec<IvrCatalogEntry> = seen.into_values().collect();
    result.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    result
}

pub fn scan_queue_catalog(queue_dir: &Path, extra_patterns: &[String]) -> Vec<QueueCatalogEntry> {
    let mut seen: HashMap<String, QueueCatalogEntry> = HashMap::new();

    if queue_dir.exists() {
        if let Ok(entries) = fs::read_dir(queue_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let file_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                if !file_name.ends_with(".toml") {
                    continue;
                }

                let generated = file_name.ends_with(".generated.toml");
                let content = match fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("failed to read queue file {}: {}", path.display(), e);
                        continue;
                    }
                };

                let multi_parsed: Result<HashMap<String, RouteQueueConfig>, _> =
                    toml::from_str(&content);
                if let Ok(map) = multi_parsed {
                    for (key, config) in map {
                        let entry_key = key.clone();
                        let entry_name = config.name.clone().unwrap_or_else(|| key.clone());
                        let existing_is_generated = seen.get(&key).map(|e| e.generated);
                        if existing_is_generated == Some(true) && !generated {
                            seen.remove(&key);
                        }
                        if seen.contains_key(&key) {
                            continue;
                        }
                        seen.insert(
                            key,
                            QueueCatalogEntry {
                                key: entry_key,
                                name: entry_name,
                                description: None,
                                file_path: path.display().to_string(),
                                generated,
                            },
                        );
                    }
                    continue;
                }

                let single_parsed: Result<QueueFileDocument, _> = toml::from_str(&content);
                if let Ok(doc) = single_parsed {
                    let doc_id = doc.id;
                    let doc_name = doc.name.clone();
                    let doc_desc = doc.description.clone();
                    let export_key = if let Some(id) = doc_id {
                        format!("db-{}", id)
                    } else {
                        canonical_queue_key(&doc_name)
                            .unwrap_or_else(|| format!("local-{}", slugify_queue_name(&doc_name)))
                    };
                    let entry_key = export_key.clone();

                    let existing_is_generated = seen.get(&export_key).map(|e| e.generated);
                    if existing_is_generated == Some(true) && !generated {
                        seen.remove(&export_key);
                    }
                    if seen.contains_key(&export_key) {
                        continue;
                    }
                    seen.insert(
                        export_key,
                        QueueCatalogEntry {
                            key: entry_key,
                            name: doc_name,
                            description: doc_desc,
                            file_path: path.display().to_string(),
                            generated,
                        },
                    );
                }
            }
        }
    }

    for pattern in extra_patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entries = match glob(trimmed) {
            Ok(e) => e,
            Err(e) => {
                warn!("invalid queue include pattern '{}': {}", trimmed, e);
                continue;
            }
        };
        for glob_entry in entries {
            let path = match glob_entry {
                Ok(p) => p,
                Err(e) => {
                    warn!("failed to read queue include glob entry: {}", e);
                    continue;
                }
            };
            if !path.is_file() {
                continue;
            }
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let generated = file_name.ends_with(".generated.toml");

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "failed to read queue include file {}: {}",
                        path.display(),
                        e
                    );
                    continue;
                }
            };

            let single_parsed: Result<QueueFileDocument, _> = toml::from_str(&content);
            if let Ok(doc) = single_parsed {
                let doc_id = doc.id;
                let doc_name = doc.name.clone();
                let doc_desc = doc.description.clone();
                let export_key = if let Some(id) = doc_id {
                    format!("db-{}", id)
                } else {
                    canonical_queue_key(&doc_name)
                        .unwrap_or_else(|| format!("local-{}", slugify_queue_name(&doc_name)))
                };
                let entry_key = export_key.clone();

                let existing_is_generated = seen.get(&export_key).map(|e| e.generated);
                if existing_is_generated == Some(true) && !generated {
                    seen.remove(&export_key);
                }
                if seen.contains_key(&export_key) {
                    continue;
                }
                seen.insert(
                    export_key,
                    QueueCatalogEntry {
                        key: entry_key,
                        name: doc_name,
                        description: doc_desc,
                        file_path: path.display().to_string(),
                        generated,
                    },
                );
            }
        }
    }

    let mut result: Vec<QueueCatalogEntry> = seen.into_values().collect();
    result.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    result
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForwardingCatalog {
    pub queues: Vec<ForwardingQueue>,
    pub ivr_projects: Vec<ForwardingIvr>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForwardingQueue {
    pub reference: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForwardingIvr {
    pub name: String,
    pub description: Option<String>,
    pub ivr_mode: String,
    pub file_path: String,
}

impl ForwardingCatalog {
    pub fn empty() -> Self {
        Self {
            queues: Vec::new(),
            ivr_projects: Vec::new(),
        }
    }
}

pub fn build_forwarding_catalog(proxy_config: &ProxyConfig) -> ForwardingCatalog {
    let mut catalog = ForwardingCatalog::empty();

    catalog.queues = scan_queue_catalog(
        &proxy_config.generated_queue_dir(),
        &proxy_config.queues_files,
    )
    .into_iter()
    .map(|q| ForwardingQueue {
        reference: q.key,
        name: q.name,
        description: q.description,
    })
    .collect();

    catalog.ivr_projects =
        scan_ivr_catalog(&proxy_config.generated_ivr_dir(), &proxy_config.ivr_files)
            .into_iter()
            .map(|i| ForwardingIvr {
                name: i.name,
                description: i.description,
                ivr_mode: i.ivr_mode,
                file_path: i.file_path,
            })
            .collect();

    catalog
}

pub fn load_proxy_config(
    app_state: Option<&Arc<crate::app::AppStateInner>>,
) -> Option<ProxyConfig> {
    app_state.map(|s| s.config().proxy.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_queue_catalog_with_multi_queue_file() {
        let dir = std::env::temp_dir().join("rustpbx_catalog_test_multi");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let toml_content = r#"
[db-2]
name = "nouser"
accept_immediately = true
passthrough_ringback = false

[db-2.strategy]
mode = "sequential"
targets = []

[db-1]
name = "toagent"
accept_immediately = true
passthrough_ringback = false

[db-1.strategy]
mode = "sequential"
"#;
        std::fs::write(dir.join("queues.generated.toml"), toml_content).unwrap();

        let result = scan_queue_catalog(&dir, &[]);
        assert_eq!(result.len(), 2, "should have 2 queues, got: {:?}", result);

        let names: Vec<&str> = result.iter().map(|q| q.name.as_str()).collect();
        assert!(
            names.contains(&"nouser"),
            "should contain nouser: {:?}",
            names
        );
        assert!(
            names.contains(&"toagent"),
            "should contain toagent: {:?}",
            names
        );

        let keys: Vec<&str> = result.iter().map(|q| q.key.as_str()).collect();
        assert!(keys.contains(&"db-1"), "should contain db-1: {:?}", keys);
        assert!(keys.contains(&"db-2"), "should contain db-2: {:?}", keys);
    }

    #[test]
    fn test_scan_ivr_catalog_with_generated_file() {
        let dir = std::env::temp_dir().join("rustpbx_catalog_test_ivr");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let toml_content = r#"
[ivr]
name = "abcd"
description = "test ivr"
ivr_mode = "tree"

[ivr.root]
greeting = "sounds/welcome.wav"
timeout_ms = 5000
max_retries = 3
entries = []
"#;
        std::fs::write(dir.join("abcd.generated.toml"), toml_content).unwrap();

        let result = scan_ivr_catalog(&dir, &[]);
        assert_eq!(result.len(), 1, "should have 1 IVR, got: {:?}", result);
        assert_eq!(result[0].name, "abcd");
        assert_eq!(result[0].ivr_mode, "tree");
        assert!(result[0].generated);
    }
}
