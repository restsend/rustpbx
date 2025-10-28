use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use glob::glob;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;
use tracing::info;

use crate::{
    config::{ProxyConfig, RecordingPolicy},
    models::{routing, sip_trunk},
    proxy::routing::{
        ConfigOrigin, DefaultRoute, DestConfig, MatchConditions, RewriteRules, RouteAction,
        RouteDirection, RouteRule, TrunkConfig,
    },
};

pub struct ProxyDataContext {
    config: Arc<ProxyConfig>,
    trunks: RwLock<HashMap<String, TrunkConfig>>,
    routes: RwLock<Vec<RouteRule>>,
    acl_rules: RwLock<Vec<String>>,
    db: Option<DatabaseConnection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReloadMetrics {
    pub total: usize,
    pub config_count: usize,
    pub file_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated: Option<GeneratedFileMetrics>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeneratedFileMetrics {
    pub entries: usize,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

impl ProxyDataContext {
    pub async fn new(config: Arc<ProxyConfig>, db: Option<DatabaseConnection>) -> Result<Self> {
        let ctx = Self {
            config,
            trunks: RwLock::new(HashMap::new()),
            routes: RwLock::new(Vec::new()),
            acl_rules: RwLock::new(Vec::new()),
            db,
        };
        let _ = ctx.reload_trunks(false).await?;
        let _ = ctx.reload_routes(false).await?;
        let _ = ctx.reload_acl_rules(false).await?;
        Ok(ctx)
    }

    pub fn config(&self) -> Arc<ProxyConfig> {
        self.config.clone()
    }

    pub fn default_route(&self) -> Option<DefaultRoute> {
        self.config.default.clone()
    }

    pub async fn trunks_snapshot(&self) -> HashMap<String, TrunkConfig> {
        self.trunks.read().await.clone()
    }

    pub async fn get_trunk(&self, name: &str) -> Option<TrunkConfig> {
        self.trunks.read().await.get(name).cloned()
    }

    pub async fn routes_snapshot(&self) -> Vec<RouteRule> {
        self.routes.read().await.clone()
    }

    pub async fn acl_rules_snapshot(&self) -> Vec<String> {
        self.acl_rules.read().await.clone()
    }

    pub async fn find_trunk_by_ip(&self, addr: &IpAddr) -> Option<String> {
        let trunks = self.trunks_snapshot().await;
        for (name, trunk) in trunks.iter() {
            if trunk.matches_inbound_ip(addr).await {
                return Some(name.clone());
            }
        }
        None
    }

    pub async fn reload_trunks(&self, generated_toml: bool) -> Result<ReloadMetrics> {
        let started_at = Utc::now();
        let default_dir = self.config.generated_trunks_dir();
        let generated = if generated_toml {
            self.export_trunks_to_toml(default_dir.as_path()).await?
        } else {
            None
        };
        let mut generated_entries = 0usize;
        if let Some(ref info) = generated {
            generated_entries = info.entries;
        }
        let mut trunks: HashMap<String, TrunkConfig> = HashMap::new();
        let mut config_count = 0usize;
        let mut file_count = 0usize;
        let mut files: Vec<String> = Vec::new();
        let patterns = self.config.trunks_files.clone();
        if !self.config.trunks.is_empty() {
            config_count = self.config.trunks.len();
            info!(count = config_count, "loading trunks from embedded config");
            for (name, trunk) in self.config.trunks.iter() {
                let mut copy = trunk.clone();
                copy.origin = ConfigOrigin::embedded();
                trunks.insert(name.clone(), copy);
            }
        }
        if !self.config.trunks_files.is_empty() {
            let (file_trunks, file_paths) = load_trunks_from_files(&self.config.trunks_files)?;
            file_count = file_trunks.len();
            if !file_paths.is_empty() {
                files.extend(file_paths);
            }
            trunks.extend(file_trunks);
        }
        if let Some(ref info) = generated {
            let generated_pattern = vec![info.path.clone()];
            let (generated_trunks, _) = load_trunks_from_files(&generated_pattern)?;
            trunks.extend(generated_trunks);
        }

        let len = trunks.len();
        *self.trunks.write().await = trunks;
        let finished_at = Utc::now();
        let duration_ms = (finished_at - started_at).num_milliseconds();
        info!(
            total = len,
            config_count, file_count, generated_entries, duration_ms, "trunks reloaded"
        );
        Ok(ReloadMetrics {
            total: len,
            config_count,
            file_count,
            generated,
            files,
            patterns,
            started_at,
            finished_at,
            duration_ms,
        })
    }

    pub async fn reload_routes(&self, generated_toml: bool) -> Result<ReloadMetrics> {
        let started_at = Utc::now();
        let default_dir = self.config.generated_routes_dir();
        let generated = if generated_toml {
            self.export_routes_to_toml(default_dir.as_path()).await?
        } else {
            None
        };
        let mut generated_entries = 0usize;
        if let Some(ref info) = generated {
            generated_entries = info.entries;
        }
        let mut routes: Vec<RouteRule> = Vec::new();
        let mut config_count = 0usize;
        let mut file_count = 0usize;
        let mut files: Vec<String> = Vec::new();
        let patterns = self.config.routes_files.clone();
        if let Some(cfg_routes) = self.config.routes.clone() {
            config_count = cfg_routes.len();
            info!(count = config_count, "loading routes from embedded config");
            for mut route in cfg_routes {
                route.origin = ConfigOrigin::embedded();
                upsert_route(&mut routes, route);
            }
        }
        if !self.config.routes_files.is_empty() {
            let (file_routes, file_paths) = load_routes_from_files(&self.config.routes_files)?;
            file_count = file_routes.len();
            if !file_paths.is_empty() {
                files.extend(file_paths);
            }
            for route in file_routes {
                upsert_route(&mut routes, route);
            }
        }
        if let Some(ref info) = generated {
            let generated_pattern = vec![info.path.clone()];
            let (generated_routes, _) = load_routes_from_files(&generated_pattern)?;
            for route in generated_routes {
                upsert_route(&mut routes, route);
            }
        }

        routes.sort_by_key(|r| r.priority);
        let len = routes.len();
        *self.routes.write().await = routes;
        let finished_at = Utc::now();
        let duration_ms = (finished_at - started_at).num_milliseconds();
        info!(
            total = len,
            config_count, file_count, generated_entries, duration_ms, "routes reloaded"
        );
        Ok(ReloadMetrics {
            total: len,
            config_count,
            file_count,
            generated,
            files,
            patterns,
            started_at,
            finished_at,
            duration_ms,
        })
    }

    pub async fn reload_acl_rules(&self, _generated_toml: bool) -> Result<ReloadMetrics> {
        let started_at = Utc::now();
        let mut rules: Vec<String> = Vec::new();
        let mut config_count = 0usize;
        let mut file_count = 0usize;
        let files_patterns = self.config.acl_files.clone();
        let mut files: Vec<String> = Vec::new();

        if let Some(cfg_rules) = self.config.acl_rules.clone() {
            config_count = cfg_rules.len();
            if config_count > 0 {
                info!(
                    count = config_count,
                    "loading acl rules from embedded config"
                );
            }
            rules.extend(cfg_rules);
        }

        if !self.config.acl_files.is_empty() {
            let (file_rules, file_paths) = load_acl_rules_from_files(&self.config.acl_files)?;
            file_count = file_rules.len();
            if !file_paths.is_empty() {
                files.extend(file_paths);
            }
            rules.extend(file_rules);
        }

        let generated_acl_path = self.config.generated_acl_dir().join("acl.generated.toml");
        if generated_acl_path.exists() {
            let generated_pattern = vec![generated_acl_path.to_string_lossy().to_string()];
            let (generated_rules, generated_files) = load_acl_rules_from_files(&generated_pattern)?;
            if !generated_files.is_empty() {
                files.extend(generated_files);
            }
            file_count += generated_rules.len();
            rules.extend(generated_rules);
        }

        if rules.is_empty() {
            rules.push("allow all".to_string());
            rules.push("deny all".to_string());
        }

        let len = rules.len();
        *self.acl_rules.write().await = rules;
        let finished_at = Utc::now();
        let duration_ms = (finished_at - started_at).num_milliseconds();
        info!(
            total = len,
            config_count, file_count, duration_ms, "acl rules reloaded"
        );
        Ok(ReloadMetrics {
            total: len,
            config_count,
            file_count,
            generated: None,
            files,
            patterns: files_patterns,
            started_at,
            finished_at,
            duration_ms,
        })
    }

    async fn export_trunks_to_toml(
        &self,
        default_dir: &Path,
    ) -> Result<Option<GeneratedFileMetrics>> {
        let Some(db) = self.db.as_ref() else {
            return Ok(None);
        };
        let Some(target_path) = resolve_generated_path(
            &self.config.trunks_files,
            default_dir,
            "trunks.generated.toml",
        ) else {
            return Ok(None);
        };

        let trunks = load_trunks_from_db(db).await?;
        let entries = trunks.len();
        let backup = backup_existing_file(&target_path)?;
        write_trunks_file(&target_path, &trunks)?;
        info!(path = %target_path.display(), entries, "generated trunks file from database");
        Ok(Some(GeneratedFileMetrics {
            entries,
            path: target_path.to_string_lossy().to_string(),
            backup: backup.map(|path| path.to_string_lossy().to_string()),
        }))
    }

    async fn export_routes_to_toml(
        &self,
        default_dir: &Path,
    ) -> Result<Option<GeneratedFileMetrics>> {
        let Some(db) = self.db.as_ref() else {
            return Ok(None);
        };
        let Some(target_path) = resolve_generated_path(
            &self.config.routes_files,
            default_dir,
            "routes.generated.toml",
        ) else {
            return Ok(None);
        };

        let trunk_lookup = {
            let guard = self.trunks.read().await;
            guard
                .iter()
                .filter_map(|(name, trunk)| trunk.id.map(|id| (id, name.clone())))
                .collect::<HashMap<i64, String>>()
        };

        let routes = load_routes_from_db(db, &trunk_lookup).await?;
        let entries = routes.len();
        let backup = backup_existing_file(&target_path)?;
        write_routes_file(&target_path, &routes)?;
        info!(path = %target_path.display(), entries, "generated routes file from database");
        Ok(Some(GeneratedFileMetrics {
            entries,
            path: target_path.to_string_lossy().to_string(),
            backup: backup.map(|path| path.to_string_lossy().to_string()),
        }))
    }
}

#[derive(Default, Deserialize, Serialize)]
struct TrunkIncludeFile {
    #[serde(default)]
    trunks: HashMap<String, TrunkConfig>,
}

#[derive(Default, Deserialize, Serialize)]
struct RouteIncludeFile {
    #[serde(default)]
    routes: Vec<RouteRule>,
}

#[derive(Default, Deserialize, Serialize)]
struct AclIncludeFile {
    #[serde(default)]
    acl_rules: Vec<String>,
}

fn load_trunks_from_files(
    patterns: &[String],
) -> Result<(HashMap<String, TrunkConfig>, Vec<String>)> {
    let mut trunks: HashMap<String, TrunkConfig> = HashMap::new();
    let mut files: Vec<String> = Vec::new();
    for pattern in patterns {
        let entries = glob(pattern)
            .map_err(|e| anyhow!("invalid trunk include pattern '{}': {}", pattern, e))?;
        for entry in entries {
            let path =
                entry.map_err(|e| anyhow!("failed to read trunk include glob entry: {}", e))?;
            let path_display = path.display().to_string();
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read trunk include file {}", path_display))?;
            let data: TrunkIncludeFile = toml::from_str(&contents)
                .with_context(|| format!("failed to parse trunk include file {}", path_display))?;
            if !files.contains(&path_display) {
                files.push(path_display.clone());
            }
            if data.trunks.is_empty() {
                info!("trunk include file {} contained no trunks", path_display);
            }
            for (name, mut trunk) in data.trunks {
                info!("loaded trunk '{}' from {}", name, path_display);
                trunk.origin = ConfigOrigin::from_file(path_display.clone());
                trunks.insert(name, trunk);
            }
        }
    }
    Ok((trunks, files))
}

fn load_routes_from_files(patterns: &[String]) -> Result<(Vec<RouteRule>, Vec<String>)> {
    let mut routes: Vec<RouteRule> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for pattern in patterns {
        let entries = glob(pattern)
            .map_err(|e| anyhow!("invalid route include pattern '{}': {}", pattern, e))?;
        for entry in entries {
            let path =
                entry.map_err(|e| anyhow!("failed to read route include glob entry: {}", e))?;
            let path_display = path.display().to_string();
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read route include file {}", path_display))?;
            let data: RouteIncludeFile = toml::from_str(&contents)
                .with_context(|| format!("failed to parse route include file {}", path_display))?;
            if !files.contains(&path_display) {
                files.push(path_display.clone());
            }
            if data.routes.is_empty() {
                info!("route include file {} contained no routes", path_display);
            }
            for mut route in data.routes {
                info!("loaded route '{}' from {}", route.name, path_display);
                route.origin = ConfigOrigin::from_file(path_display.clone());
                upsert_route(&mut routes, route);
            }
        }
    }
    Ok((routes, files))
}

fn load_acl_rules_from_files(patterns: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut rules: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for pattern in patterns {
        let entries = glob(pattern)
            .map_err(|e| anyhow!("invalid acl include pattern '{}': {}", pattern, e))?;
        for entry in entries {
            let path =
                entry.map_err(|e| anyhow!("failed to read acl include glob entry: {}", e))?;
            let path_display = path.display().to_string();
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read acl include file {}", path_display))?;
            let data: AclIncludeFile = toml::from_str(&contents)
                .with_context(|| format!("failed to parse acl include file {}", path_display))?;
            if !files.contains(&path_display) {
                files.push(path_display.clone());
            }
            if data.acl_rules.is_empty() {
                info!("acl include file {} contained no rules", path_display);
            }
            for rule in data.acl_rules {
                info!("loaded acl rule '{}' from {}", rule, path_display);
                rules.push(rule);
            }
        }
    }
    Ok((rules, files))
}

fn upsert_route(routes: &mut Vec<RouteRule>, route: RouteRule) {
    info!("upserted route '{}'", route.name);
    if let Some(idx) = routes
        .iter()
        .position(|existing| existing.name == route.name)
    {
        routes[idx] = route;
    } else {
        routes.push(route);
    }
}

fn contains_glob_chars(value: &str) -> bool {
    value
        .chars()
        .any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
}

fn resolve_generated_path(
    patterns: &[String],
    default_dir: &Path,
    default_name: &str,
) -> Option<PathBuf> {
    for pattern in patterns {
        if pattern.trim().is_empty() {
            continue;
        }
        let path = Path::new(pattern);
        if contains_glob_chars(pattern) {
            if let Some(parent) = path.parent() {
                if parent.as_os_str().is_empty() {
                    return Some(default_dir.join(default_name));
                }
                return Some(parent.to_path_buf().join(default_name));
            }
            return Some(default_dir.join(default_name));
        } else {
            return Some(path.to_path_buf());
        }
    }
    Some(default_dir.join(default_name))
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

fn backup_existing_file(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    let backup_name = format!("{}.{}.bak", file_name, timestamp);
    let backup_path = path.with_file_name(backup_name);
    fs::rename(path, &backup_path).with_context(|| {
        format!(
            "failed to backup {} to {}",
            path.display(),
            backup_path.display()
        )
    })?;
    Ok(Some(backup_path))
}

fn write_trunks_file(path: &Path, trunks: &HashMap<String, TrunkConfig>) -> Result<()> {
    ensure_parent_dir(path)?;
    let mut data = TrunkIncludeFile::default();
    data.trunks = trunks
        .iter()
        .map(|(name, trunk)| (name.clone(), trunk.clone()))
        .collect();
    let toml = toml::to_string_pretty(&data)
        .with_context(|| format!("failed to serialize trunks toml for {}", path.display()))?;
    fs::write(path, toml)
        .with_context(|| format!("failed to write trunks file {}", path.display()))?;
    Ok(())
}

fn write_routes_file(path: &Path, routes: &[RouteRule]) -> Result<()> {
    ensure_parent_dir(path)?;
    let mut data = RouteIncludeFile::default();
    data.routes = routes.to_vec();
    let toml = toml::to_string_pretty(&data)
        .with_context(|| format!("failed to serialize routes toml for {}", path.display()))?;
    fs::write(path, toml)
        .with_context(|| format!("failed to write routes file {}", path.display()))?;
    Ok(())
}

async fn load_trunks_from_db(db: &DatabaseConnection) -> Result<HashMap<String, TrunkConfig>> {
    let models = sip_trunk::Entity::find()
        .filter(sip_trunk::Column::IsActive.eq(true))
        .order_by_asc(sip_trunk::Column::Name)
        .all(db)
        .await?;

    let mut trunks = HashMap::new();
    for model in models {
        if let Some((name, trunk)) = convert_trunk(model) {
            trunks.insert(name, trunk);
        }
    }
    Ok(trunks)
}

fn convert_trunk(model: sip_trunk::Model) -> Option<(String, TrunkConfig)> {
    let primary = model.sip_server.clone().or(model.outbound_proxy.clone());
    let dest = primary?;

    let backup_dest = if let Some(outbound) = model.outbound_proxy.clone() {
        if outbound != dest {
            Some(outbound)
        } else {
            None
        }
    } else {
        None
    };

    let transport = Some(model.sip_transport.as_str().to_string());

    let mut inbound_hosts = extract_string_array(model.allowed_ips.clone());
    if let Some(host) = extract_host_from_uri(&dest) {
        if host.parse::<IpAddr>().is_ok() {
            push_unique(&mut inbound_hosts, host);
        }
    }
    if let Some(backup) = &backup_dest {
        if let Some(host) = extract_host_from_uri(backup) {
            if host.parse::<IpAddr>().is_ok() {
                push_unique(&mut inbound_hosts, host);
            }
        }
    }

    let recording = model
        .metadata
        .as_ref()
        .and_then(recording_policy_from_metadata);

    let trunk = TrunkConfig {
        dest,
        backup_dest,
        username: model.auth_username,
        password: model.auth_password,
        codec: Vec::new(),
        disabled: Some(!model.is_active),
        max_calls: model.max_concurrent.map(|v| v as u32),
        max_cps: model.max_cps.map(|v| v as u32),
        weight: None,
        transport,
        id: Some(model.id),
        direction: Some(model.direction.into()),
        inbound_hosts,
        recording,
        origin: ConfigOrigin::embedded(),
    };

    Some((model.name, trunk))
}

pub(crate) async fn load_routes_from_db(
    db: &DatabaseConnection,
    trunk_lookup: &HashMap<i64, String>,
) -> Result<Vec<RouteRule>> {
    let models = routing::Entity::find()
        .filter(routing::Column::IsActive.eq(true))
        .order_by_asc(routing::Column::Priority)
        .all(db)
        .await?;

    let mut routes = Vec::new();
    for model in models {
        if let Some(route) = convert_route(model, trunk_lookup).context("convert route")? {
            routes.push(route);
        }
    }
    Ok(routes)
}

fn recording_policy_from_metadata(value: &serde_json::Value) -> Option<RecordingPolicy> {
    value
        .get("recording")
        .and_then(|entry| serde_json::from_value::<RecordingPolicy>(entry.clone()).ok())
}

fn convert_route(
    model: routing::Model,
    trunk_lookup: &HashMap<i64, String>,
) -> Result<Option<RouteRule>> {
    let mut match_conditions = MatchConditions::default();
    if let Some(pattern) = model.source_pattern.clone() {
        if !pattern.is_empty() {
            match_conditions.from_user = Some(pattern);
        }
    }
    if let Some(pattern) = model.destination_pattern.clone() {
        if !pattern.is_empty() {
            match_conditions.to_user = Some(pattern);
        }
    }

    if let Some(filters) = model.header_filters.clone() {
        if let Ok(map) = serde_json::from_value::<HashMap<String, String>>(filters) {
            apply_match_filters(&mut match_conditions, map);
        }
    }
    finalize_match_conditions(&mut match_conditions);

    let rewrite_rules = model
        .rewrite_rules
        .clone()
        .and_then(|value| serde_json::from_value::<RewriteRules>(value).ok())
        .map(|mut rules| {
            normalize_rewrite_rules(&mut rules);
            rules
        });

    let target_trunks: Vec<String> = model
        .target_trunks
        .clone()
        .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
        .unwrap_or_default();

    let dest = if target_trunks.is_empty() {
        None
    } else if target_trunks.len() == 1 {
        Some(DestConfig::Single(target_trunks[0].clone()))
    } else {
        Some(DestConfig::Multiple(target_trunks))
    };

    let mut action = RouteAction::default();
    if let Some(dest) = dest {
        action.dest = Some(dest);
    }
    action.select = model.selection_strategy.as_str().to_string();
    action.hash_key = model.hash_key.clone();

    let direction = match model.direction {
        routing::RoutingDirection::Inbound => RouteDirection::Inbound,
        routing::RoutingDirection::Outbound => RouteDirection::Outbound,
    };

    let mut source_trunks = Vec::new();
    let mut source_trunk_ids = Vec::new();
    if let Some(id) = model.source_trunk_id {
        source_trunk_ids.push(id);
        if let Some(name) = trunk_lookup.get(&id) {
            source_trunks.push(name.clone());
        }
    }

    let route = RouteRule {
        name: model.name,
        description: model.description,
        priority: model.priority,
        direction,
        source_trunks,
        source_trunk_ids,
        match_conditions,
        rewrite: rewrite_rules,
        action,
        disabled: Some(!model.is_active),
        origin: ConfigOrigin::embedded(),
    };
    Ok(Some(route))
}

fn set_field(target: &mut Option<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    match target {
        Some(existing) if existing == trimmed => {}
        _ => *target = Some(trimmed.to_string()),
    }
}

fn canonical_condition_key(raw: &str) -> String {
    raw.trim()
        .to_ascii_lowercase()
        .replace('_', ".")
        .replace('-', ".")
}

fn handle_match_key(match_conditions: &mut MatchConditions, key: &str, value: &str) -> bool {
    let trimmed_key = key.trim();
    if trimmed_key.is_empty() {
        return true;
    }
    let canonical = canonical_condition_key(trimmed_key);
    match canonical.as_str() {
        "from.user" | "caller" | "from" => {
            set_field(&mut match_conditions.from_user, value);
            true
        }
        "from.host" => {
            set_field(&mut match_conditions.from_host, value);
            true
        }
        "to.user" | "callee" | "to" => {
            set_field(&mut match_conditions.to_user, value);
            true
        }
        "to.host" => {
            set_field(&mut match_conditions.to_host, value);
            true
        }
        "to.port" => {
            set_field(&mut match_conditions.to_port, value);
            true
        }
        "request.uri.user" => {
            set_field(&mut match_conditions.request_uri_user, value);
            true
        }
        "request.uri.host" => {
            set_field(&mut match_conditions.request_uri_host, value);
            true
        }
        "request.uri.port" => {
            set_field(&mut match_conditions.request_uri_port, value);
            true
        }
        _ => false,
    }
}

fn apply_match_filters(match_conditions: &mut MatchConditions, map: HashMap<String, String>) {
    let mut headers = HashMap::new();
    for (key, raw_value) in map {
        let value = raw_value.trim();
        if value.is_empty() {
            continue;
        }
        if handle_match_key(match_conditions, &key, value) {
            continue;
        }
        headers.insert(key.trim().to_string(), value.to_string());
    }
    match_conditions.headers = headers;
}

fn finalize_match_conditions(match_conditions: &mut MatchConditions) {
    if let Some(value) = match_conditions.from.take() {
        set_field(&mut match_conditions.from_user, value.as_str());
    }
    if let Some(value) = match_conditions.caller.take() {
        set_field(&mut match_conditions.from_user, value.as_str());
    }
    if let Some(value) = match_conditions.to.take() {
        set_field(&mut match_conditions.to_user, value.as_str());
    }
    if let Some(value) = match_conditions.callee.take() {
        set_field(&mut match_conditions.to_user, value.as_str());
    }

    let entries = std::mem::take(&mut match_conditions.headers);
    for (key, raw_value) in entries {
        let trimmed_key = key.trim();
        if trimmed_key.is_empty() {
            continue;
        }
        let value = raw_value.trim();
        if value.is_empty() {
            continue;
        }
        if handle_match_key(match_conditions, trimmed_key, value) {
            continue;
        }
        match_conditions
            .headers
            .insert(trimmed_key.to_string(), value.to_string());
    }
}

fn handle_rewrite_key(rules: &mut RewriteRules, key: &str, value: &str) -> bool {
    let trimmed_key = key.trim();
    if trimmed_key.is_empty() {
        return true;
    }
    let canonical = canonical_condition_key(trimmed_key);
    match canonical.as_str() {
        "from.user" => {
            set_field(&mut rules.from_user, value);
            true
        }
        "from.host" => {
            set_field(&mut rules.from_host, value);
            true
        }
        "to.user" => {
            set_field(&mut rules.to_user, value);
            true
        }
        "to.host" => {
            set_field(&mut rules.to_host, value);
            true
        }
        "to.port" => {
            set_field(&mut rules.to_port, value);
            true
        }
        "request.uri.user" => {
            set_field(&mut rules.request_uri_user, value);
            true
        }
        "request.uri.host" => {
            set_field(&mut rules.request_uri_host, value);
            true
        }
        "request.uri.port" => {
            set_field(&mut rules.request_uri_port, value);
            true
        }
        _ => false,
    }
}

fn normalize_rewrite_rules(rules: &mut RewriteRules) {
    let mut headers = HashMap::new();
    let existing = std::mem::take(&mut rules.headers);
    for (key, raw_value) in existing {
        let value = raw_value.trim();
        if value.is_empty() {
            continue;
        }
        if handle_rewrite_key(rules, &key, value) {
            continue;
        }
        headers.insert(key.trim().to_string(), value.to_string());
    }
    rules.headers = headers;
}

fn extract_string_array(value: Option<serde_json::value::Value>) -> Vec<String> {
    match value {
        Some(json) => match json {
            serde_json::Value::Array(items) => items
                .into_iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            serde_json::Value::String(s) => vec![s],
            _ => Vec::new(),
        },
        None => Vec::new(),
    }
}

fn extract_host_from_uri(uri: &str) -> Option<String> {
    rsip::Uri::try_from(uri)
        .ok()
        .map(|parsed| parsed.host_with_port.host.to_string())
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.iter().any(|existing| existing == &value) {
        list.push(value);
    }
}
