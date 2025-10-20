use std::{collections::HashMap, fs, net::IpAddr, sync::Arc};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use glob::glob;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use tracing::info;

use crate::{
    config::{ProxyConfig, ProxyDataSource},
    models::{routing, sip_trunk},
    proxy::routing::{
        DefaultRoute, DestConfig, MatchConditions, RewriteRules, RouteAction, RouteDirection,
        RouteRule, TrunkConfig,
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
    pub db_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: i64,
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
        let _ = ctx.reload_trunks().await?;
        let _ = ctx.reload_routes().await?;
        let _ = ctx.reload_acl_rules().await?;
        Ok(ctx)
    }

    pub fn config(&self) -> Arc<ProxyConfig> {
        self.config.clone()
    }

    pub fn trunks_source(&self) -> ProxyDataSource {
        self.config.trunks_source
    }

    pub fn routes_source(&self) -> ProxyDataSource {
        self.config.routes_source
    }

    pub fn default_route(&self) -> Option<DefaultRoute> {
        self.config.default.clone()
    }

    pub async fn trunks_snapshot(&self) -> HashMap<String, TrunkConfig> {
        self.trunks.read().await.clone()
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

    pub async fn reload_trunks(&self) -> Result<ReloadMetrics> {
        let started_at = Utc::now();
        let mut trunks: HashMap<String, TrunkConfig> = HashMap::new();
        let mut config_count = 0usize;
        let mut file_count = 0usize;
        let mut db_count = 0usize;
        let mut files: Vec<String> = Vec::new();
        let patterns = self.config.trunks_files.clone();
        if self.config.trunks_source != ProxyDataSource::Database {
            config_count = self.config.trunks.len();
            if config_count > 0 {
                info!(count = config_count, "loading trunks from embedded config");
            }
            trunks.extend(self.config.trunks.clone());
            if !self.config.trunks_files.is_empty() {
                let (file_trunks, file_paths) = load_trunks_from_files(&self.config.trunks_files)?;
                file_count = file_trunks.len();
                if !file_paths.is_empty() {
                    files.extend(file_paths);
                }
                trunks.extend(file_trunks);
            }
        }

        if self.config.trunks_source != ProxyDataSource::Config {
            let db = self
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("Database connection required for trunk reload"))?;
            let db_trunks = load_trunks_from_db(db).await?;
            db_count = db_trunks.len();
            trunks.extend(db_trunks);
        }

        let len = trunks.len();
        *self.trunks.write().await = trunks;
        let finished_at = Utc::now();
        let duration_ms = (finished_at - started_at).num_milliseconds();
        info!(
            total = len,
            config_count, file_count, db_count, duration_ms, "trunks reloaded"
        );
        Ok(ReloadMetrics {
            total: len,
            config_count,
            file_count,
            db_count,
            files,
            patterns,
            started_at,
            finished_at,
            duration_ms,
        })
    }

    pub async fn reload_routes(&self) -> Result<ReloadMetrics> {
        let started_at = Utc::now();
        let mut routes: Vec<RouteRule> = Vec::new();
        let mut config_count = 0usize;
        let mut file_count = 0usize;
        let mut db_count = 0usize;
        let mut files: Vec<String> = Vec::new();
        let patterns = self.config.routes_files.clone();
        if self.config.routes_source != ProxyDataSource::Database {
            if let Some(cfg_routes) = self.config.routes.clone() {
                config_count = cfg_routes.len();
                if config_count > 0 {
                    info!(count = config_count, "loading routes from embedded config");
                }
                for route in cfg_routes {
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
        }

        if self.config.routes_source != ProxyDataSource::Config {
            let db = self
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("Database connection required for routing reload"))?;
            let trunk_id_map = {
                let trunks_guard = self.trunks.read().await;
                trunks_guard
                    .iter()
                    .filter_map(|(name, trunk)| trunk.id.map(|id| (id, name.clone())))
                    .collect::<HashMap<i64, String>>()
            };
            let db_routes = load_routes_from_db(db, &trunk_id_map).await?;
            db_count = db_routes.len();
            for route in db_routes {
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
            config_count, file_count, db_count, duration_ms, "routes reloaded"
        );
        Ok(ReloadMetrics {
            total: len,
            config_count,
            file_count,
            db_count,
            files,
            patterns,
            started_at,
            finished_at,
            duration_ms,
        })
    }

    pub async fn reload_acl_rules(&self) -> Result<ReloadMetrics> {
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
            db_count: 0,
            files,
            patterns: files_patterns,
            started_at,
            finished_at,
            duration_ms,
        })
    }
}

#[derive(Default, Deserialize)]
struct TrunkIncludeFile {
    #[serde(default)]
    trunks: HashMap<String, TrunkConfig>,
}

#[derive(Default, Deserialize)]
struct RouteIncludeFile {
    #[serde(default)]
    routes: Vec<RouteRule>,
}

#[derive(Default, Deserialize)]
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
            for (name, trunk) in data.trunks {
                info!("loaded trunk '{}' from {}", name, path_display);
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
            for route in data.routes {
                info!("loaded route '{}' from {}", route.name, path_display);
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
    if let Some(idx) = routes
        .iter()
        .position(|existing| existing.name == route.name)
    {
        routes[idx] = route;
    } else {
        routes.push(route);
    }
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
    };

    Some((model.name, trunk))
}

async fn load_routes_from_db(
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

fn convert_route(
    model: routing::Model,
    trunk_lookup: &HashMap<i64, String>,
) -> Result<Option<RouteRule>> {
    let mut match_conditions = MatchConditions::default();
    if let Some(pattern) = model.source_pattern.clone() {
        if !pattern.is_empty() {
            match_conditions.from_user = Some(pattern.clone());
            match_conditions.caller = Some(pattern);
        }
    }
    if let Some(pattern) = model.destination_pattern.clone() {
        if !pattern.is_empty() {
            match_conditions.to_user = Some(pattern.clone());
            match_conditions.callee = Some(pattern);
        }
    }

    if let Some(filters) = model.header_filters.clone() {
        if let Ok(map) = serde_json::from_value::<HashMap<String, String>>(filters) {
            match_conditions.headers = map;
        }
    }

    let rewrite_rules = model
        .rewrite_rules
        .clone()
        .and_then(|value| serde_json::from_value::<RewriteRules>(value).ok());

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
    };
    Ok(Some(route))
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
