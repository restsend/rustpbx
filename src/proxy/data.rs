use std::{collections::HashMap, net::IpAddr, sync::Arc};

use anyhow::{Context, Result, anyhow};
use tokio::sync::RwLock;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use crate::{
    config::{ProxyConfig, ProxyDataSource},
    models::{routing, sip_trunk},
    proxy::routing::{
        DefaultRoute, DestConfig, MatchConditions, RewriteRules, RouteAction, RouteRule,
        TrunkConfig,
    },
};

pub struct ProxyDataContext {
    config: Arc<ProxyConfig>,
    trunks: RwLock<HashMap<String, TrunkConfig>>,
    routes: RwLock<Vec<RouteRule>>,
    db: Option<DatabaseConnection>,
}

impl ProxyDataContext {
    pub async fn new(config: Arc<ProxyConfig>, db: Option<DatabaseConnection>) -> Result<Self> {
        let ctx = Self {
            config,
            trunks: RwLock::new(HashMap::new()),
            routes: RwLock::new(Vec::new()),
            db,
        };
        ctx.reload_trunks().await?;
        ctx.reload_routes().await?;
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

    pub async fn find_trunk_by_ip(&self, addr: &IpAddr) -> Option<String> {
        let trunks = self.trunks.read().await;
        for (name, trunk) in trunks.iter() {
            if let Ok(uri) = rsip::Uri::try_from(trunk.dest.as_str()) {
                if uri.host().to_string() == addr.to_string() {
                    return Some(name.clone());
                }
            }
        }
        None
    }

    pub async fn reload_trunks(&self) -> Result<usize> {
        let mut trunks: HashMap<String, TrunkConfig> = HashMap::new();
        if self.config.trunks_source != ProxyDataSource::Database {
            trunks.extend(self.config.trunks.clone());
        }

        if self.config.trunks_source != ProxyDataSource::Config {
            let db = self
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("Database connection required for trunk reload"))?;
            let db_trunks = load_trunks_from_db(db).await?;
            trunks.extend(db_trunks);
        }

        let len = trunks.len();
        *self.trunks.write().await = trunks;
        Ok(len)
    }

    pub async fn reload_routes(&self) -> Result<usize> {
        let mut routes: Vec<RouteRule> = Vec::new();
        if self.config.routes_source != ProxyDataSource::Database {
            if let Some(cfg_routes) = self.config.routes.clone() {
                routes.extend(cfg_routes);
            }
        }

        if self.config.routes_source != ProxyDataSource::Config {
            let db = self
                .db
                .as_ref()
                .ok_or_else(|| anyhow!("Database connection required for routing reload"))?;
            let mut db_routes = load_routes_from_db(db).await?;
            routes.append(&mut db_routes);
        }

        routes.sort_by_key(|r| r.priority);
        let len = routes.len();
        *self.routes.write().await = routes;
        Ok(len)
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
    };

    Some((model.name, trunk))
}

async fn load_routes_from_db(db: &DatabaseConnection) -> Result<Vec<RouteRule>> {
    let models = routing::Entity::find()
        .filter(routing::Column::IsActive.eq(true))
        .order_by_asc(routing::Column::Priority)
        .all(db)
        .await?;

    let mut routes = Vec::new();
    for model in models {
        if let Some(route) = convert_route(model).context("convert route")? {
            routes.push(route);
        }
    }
    Ok(routes)
}

fn convert_route(model: routing::Model) -> Result<Option<RouteRule>> {
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

    let route = RouteRule {
        name: model.name,
        description: model.description,
        priority: model.priority,
        match_conditions,
        rewrite: rewrite_rules,
        action,
        disabled: Some(!model.is_active),
    };
    Ok(Some(route))
}
