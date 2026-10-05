use crate::addons::Addon;
use crate::addons::export_reload::ExportReloadRegistry;
use crate::app::AppState;
use std::sync::Arc;

/// Replace `/static` prefix in a URL with the configured static_path.
fn normalize_static_url(url: &str, config: &crate::config::Config) -> String {
    let prefix = config.static_path();
    if url.starts_with("/static/") && prefix != "/static" {
        format!("{}{}", prefix, &url["/static".len()..])
    } else {
        url.to_string()
    }
}

pub struct AddonRegistry {
    addons: Vec<Arc<dyn Addon>>,
    pub export_reload: ExportReloadRegistry,
}

impl Default for AddonRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AddonRegistry {
    pub fn new() -> Self {
        Self::with_extra_addons(Vec::new())
    }

    pub fn with_extra_addons(extra: Vec<Arc<dyn Addon>>) -> Self {
        let mut addons: Vec<Arc<dyn Addon>> = Vec::new();

        // Observability addon (/metrics + /healthz)
        #[cfg(feature = "addon-observability")]
        {
            super::observability::ObservabilityAddon::install_recorder().ok();
            addons.push(Arc::new(super::observability::ObservabilityAddon::new()));
        }

        // ACME Addon (Free/Built-in for now as per request)
        #[cfg(feature = "addon-acme")]
        addons.push(Arc::new(super::acme::AcmeAddon::new()));

        // Archive Addon
        #[cfg(feature = "addon-archive")]
        addons.push(Arc::new(super::archive::ArchiveAddon::new()));

        // Wholesale Addon
        #[cfg(feature = "addon-wholesale")]
        addons.push(Arc::new(super::wholesale::WholesaleAddon::new()));

        // Transcript Addon
        #[cfg(feature = "addon-transcript")]
        addons.push(Arc::new(super::transcript::TranscriptAddon::new()));

        // Voicemail Addon (Commercial)
        #[cfg(feature = "addon-voicemail")]
        addons.push(Arc::new(super::voicemail::VoicemailAddon::new()));

        // IVR Editor Addon (Commercial)
        #[cfg(feature = "addon-ivr-editor")]
        addons.push(Arc::new(super::ivr_editor::IvrEditorAddon::new()));

        // Queue Addon
        addons.push(Arc::new(super::queue::QueueAddon::new()));

        // CC Addon (Contact Center)
        #[cfg(feature = "addon-cc")]
        addons.push(Arc::new(super::cc::CcAddon::new()));

        addons.extend(extra);

        // Collect export/reload handlers from addons (not gated by feature)
        let mut export_reload = ExportReloadRegistry::default();
        for addon in &addons {
            if let Some(handler) = addon.export_reload_handler() {
                export_reload.register(handler);
            }
        }

        Self {
            addons,
            export_reload,
        }
    }

    pub async fn initialize_all(&self, state: AppState) -> anyhow::Result<()> {
        let config = state.config();
        let commercial_ids: Vec<String> = self
            .addons
            .iter()
            .filter(|a| a.category() == crate::addons::AddonCategory::Commercial)
            .map(|a| a.id().to_string())
            .collect();
        if !commercial_ids.is_empty() {
            let results =
                crate::license::check_all_addon_licenses(&commercial_ids, &config.licenses).await;
            let enforce = config
                .licenses
                .as_ref()
                .map(|c| c.enforce)
                .unwrap_or(false);
            if enforce {
                for (id, status) in &results {
                    if status.expired {
                        tracing::warn!("Addon {} license expired", id);
                    } else if status.valid {
                        if let Some(days) = status.days_until_expiry()
                            && days <= 30
                        {
                            tracing::warn!("Addon {} license expires in {} days", id, days);
                        }
                    }
                }
            }
            crate::license::record_startup_results(results);
        }
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                tracing::info!("Addon {} is disabled", addon.name());
                continue;
            }
            tracing::info!("Initializing addon: {}", addon.name());
            if let Err(e) = addon.initialize(state.clone()).await {
                tracing::error!("Failed to initialize addon {}: {}", addon.name(), e);
            }
        }
        Ok(())
    }

    pub async fn seed_all_fixtures(&self, state: AppState) -> anyhow::Result<()> {
        let config = state.config();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Err(e) = addon.seed_fixtures(state.clone()).await {
                tracing::error!("Failed to seed fixtures for addon {}: {}", addon.name(), e);
            }
        }
        Ok(())
    }

    pub fn get_routers(&self, state: AppState) -> axum::Router {
        let config = state.config();
        let mut router = axum::Router::new();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Some(r) = addon.router(state.clone()) {
                router = router.merge(r);
            }
        }
        router
    }

    /// Collect AMI-relative routes from enabled addons (merged under AMI auth).
    pub fn get_ami_routes(&self, state: AppState) -> axum::Router<AppState> {
        let config = state.config();
        let mut router = axum::Router::new();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Some(r) = addon.ami_routes(state.clone()) {
                router = router.merge(r);
            }
        }
        router
    }
}

/// Merge every compiled-in addon's error catalog into `reg`.
///
/// Prefer [`AddonRegistry::merge_error_catalogs_into`] when a live registry is
/// available (app startup). This free function remains for tests that cannot
/// construct a full registry.
#[cfg(any(feature = "addon-wholesale", test))]
pub fn merge_compiled_addon_error_catalogs(reg: &mut crate::call_errors::CallErrRegistry) {
    let _ = &reg;
    #[cfg(feature = "addon-wholesale")]
    {
        use super::Addon;
        reg.merge_slice(super::wholesale::WholesaleAddon::new().error_catalog());
    }
}

impl AddonRegistry {
    pub fn get_injected_scripts(&self, path: &str, config: &crate::config::Config) -> Vec<String> {
        let mut scripts = Vec::new();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            for injection in addon.inject_scripts() {
                if let Ok(re) = regex::Regex::new(injection.url_path_regex)
                    && re.is_match(path)
                {
                    scripts.push(normalize_static_url(&injection.script_url, config));
                }
            }
        }
        scripts
    }

    pub fn get_sidebar_items(&self, state: AppState) -> Vec<super::SidebarItem> {
        let config = state.config();
        self.addons
            .iter()
            .filter(|a| self.is_enabled(a.id(), config))
            .flat_map(|a| a.sidebar_items(state.clone()))
            .collect()
    }

    pub fn get_template_dirs(&self, config: &crate::config::Config) -> Vec<String> {
        self.addons
            .iter()
            .filter(|a| self.is_enabled(a.id(), config))
            .flat_map(|a| {
                let mut dirs = Vec::with_capacity(3);
                if let Some(dir) = a.template_dir() {
                    dirs.push(dir);
                }
                dirs.push(format!("src/addons/{}/templates", a.id()));
                dirs.push(format!("templates/{}", a.id()));
                dirs
            })
            .collect()
    }

    /// Static asset mounts for enabled addons declaring a static_dir,
    /// as (addon_id, directory) pairs served under `/static/<addon_id>/`.
    pub fn get_static_mounts(&self, config: &crate::config::Config) -> Vec<(String, String)> {
        self.addons
            .iter()
            .filter(|a| self.is_enabled(a.id(), config))
            .filter_map(|a| a.static_dir().map(|dir| (a.id().to_string(), dir)))
            .collect()
    }

    /// Return locale directories for all addons that provide translations (not just enabled ones).
    /// This is needed because the admin UI needs to display all addons even if not enabled.
    pub fn get_locale_dirs(&self, _state: AppState) -> Vec<(String, String)> {
        self.addons
            .iter()
            .filter_map(|a| a.locales_dir().map(|dir| (a.id().to_string(), dir)))
            .collect()
    }

    pub fn list_addons(&self, state: AppState) -> Vec<super::AddonInfo> {
        let config = state.config().clone();
        self.addons
            .iter()
            .map(|a| {
                let config_url = a.config_url(state.clone());
                let license = crate::license::get_license_status(a.id());

                super::AddonInfo {
                    id: a.id().to_string(),
                    name: a.name().to_string(),
                    description: a.description().to_string(),
                    enabled: false, // Caller should set this
                    config_url,
                    category: a.category(),
                    bundle: a.bundle().map(|s| s.to_string()),
                    developer: a.developer().to_string(),
                    website: a.website().to_string(),
                    cost: a.cost().to_string(),
                    screenshots: a
                        .screenshots()
                        .iter()
                        .map(|s| normalize_static_url(s, &config))
                        .collect(),
                    restart_required: false, // Caller should set this
                    license_status: license.as_ref().map(|s| {
                        if !s.valid {
                            "Invalid".to_string()
                        } else if s.expired {
                            "Expired".to_string()
                        } else if s.is_trial {
                            "Trial".to_string()
                        } else {
                            "Valid".to_string()
                        }
                    }),
                    license_expiry: license.as_ref().and_then(|s| s.expiry.clone()),
                    license_plan: license.as_ref().map(|s| s.plan.clone()),
                    license_days_left: license.as_ref().and_then(|s| s.days_until_expiry()),
                    license_expiring_soon: license
                        .as_ref()
                        .map(|s| crate::license::expiring_soon(s, 30))
                        .unwrap_or(false),
                }
            })
            .collect()
    }

    pub fn has_commercial(&self) -> bool {
        self.addons
            .iter()
            .any(|a| a.category() == crate::addons::AddonCategory::Commercial)
    }

    pub fn is_enabled(&self, id: &str, config: &crate::config::Config) -> bool {
        let listed = config
            .proxy
            .addons
            .as_ref()
            .map(|addons| addons.iter().any(|a| a == id))
            .unwrap_or(false);
        if !listed {
            return false;
        }
        match crate::license::get_license_status(id) {
            Some(status) => status.valid,
            None => true,
        }
    }

    /// Collect routing-stack metadata from enabled addons.
    pub fn routing_contributions(
        &self,
        config: &crate::config::Config,
    ) -> Vec<crate::proxy::routing::stack::RoutingContribution> {
        let mut out = Vec::new();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            out.extend(addon.routing_contributions(config));
        }
        out
    }

    // ── Console route hooks ──────────────────────────────────────────────────

    /// Collect console page routes from all enabled addons.
    #[cfg(feature = "console")]
    pub fn get_console_page_routes(
        &self,
        state: &crate::console::ConsoleState,
        config: &crate::config::Config,
    ) -> Vec<axum::Router<Arc<crate::console::ConsoleState>>> {
        let mut routers = Vec::new();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Some(r) = addon.console_page_routes(state) {
                routers.push(r);
            }
        }
        routers
    }

    /// Collect console API routes from addons.
    ///
    /// Mounted when the addon is enabled in `[proxy].addons`, or when the addon
    /// opts into [`Addon::console_api_always_mounted`] (e.g. queue).
    #[cfg(feature = "console")]
    pub fn get_console_api_routes(
        &self,
        state: &crate::console::ConsoleState,
        config: &crate::config::Config,
    ) -> Vec<axum::Router<Arc<crate::console::ConsoleState>>> {
        let mut routers = Vec::new();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) && !addon.console_api_always_mounted() {
                continue;
            }
            if let Some(r) = addon.console_api_routes(state) {
                routers.push(r);
            }
        }
        routers
    }

    /// Ask every compiled-in addon to promote cookie extensions onto `dialplan`.
    /// Independent of `[proxy].addons` enablement — matches prior
    /// `cfg(feature = "addon-…")` behavior for CDR early-reject paths.
    pub fn promote_cookie_extensions(
        &self,
        cookie: &crate::call::TransactionCookie,
        dialplan: &mut crate::call::Dialplan,
    ) {
        for addon in &self.addons {
            addon.promote_cookie_extensions(cookie, dialplan);
        }
    }

    /// Merge each addon's error catalog into `reg` (live registry instance).
    pub fn merge_error_catalogs_into(&self, reg: &mut crate::call_errors::CallErrRegistry) {
        for addon in &self.addons {
            let cat = addon.error_catalog();
            if !cat.is_empty() {
                reg.merge_slice(cat);
            }
        }
    }

    /// Collect update-check phone-home params from all addons.
    pub async fn collect_update_check_params(
        &self,
        state: &crate::app::AppState,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for addon in &self.addons {
            out.extend(addon.update_check_params(state).await);
        }
        out
    }

    /// First non-empty tenant list from any addon (wholesale).
    pub async fn list_trunk_tenants(
        &self,
        db: &sea_orm::DatabaseConnection,
    ) -> Vec<serde_json::Value> {
        for addon in &self.addons {
            let list = addon.list_trunk_tenants(db).await;
            if !list.is_empty() {
                return list;
            }
        }
        Vec::new()
    }

    pub async fn get_trunk_tenant_id(
        &self,
        db: &sea_orm::DatabaseConnection,
        trunk_id: i64,
    ) -> Option<i64> {
        for addon in &self.addons {
            if let Some(id) = addon.get_trunk_tenant_id(db, trunk_id).await {
                return Some(id);
            }
        }
        None
    }

    pub async fn set_trunk_tenant(
        &self,
        db: &sea_orm::DatabaseConnection,
        trunk_id: i64,
        tenant_id: Option<i64>,
        clear: bool,
    ) -> Result<(), sea_orm::DbErr> {
        for addon in &self.addons {
            addon
                .set_trunk_tenant(db, trunk_id, tenant_id, clear)
                .await?;
        }
        Ok(())
    }

    /// First metrics endpoint info from any addon (observability).
    pub fn metrics_endpoint_info(
        &self,
        config_path: &Option<String>,
    ) -> Option<crate::metrics::MetricsEndpointInfo> {
        for addon in &self.addons {
            if let Some(info) = addon.metrics_endpoint_info(config_path) {
                return Some(info);
            }
        }
        None
    }

    /// Get the first phone auth token validator from any enabled addon.
    #[cfg(feature = "console")]
    pub fn get_phone_auth_validator(
        &self,
        state: &crate::console::ConsoleState,
        config: &crate::config::Config,
    ) -> Option<crate::auth::DynTokenValidator> {
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Some(validator) = addon.phone_auth_validator(state) {
                return Some(validator);
            }
        }
        None
    }

    pub fn get_call_record_hooks(
        &self,
        config: &crate::config::Config,
        db: &sea_orm::DatabaseConnection,
    ) -> Vec<Box<dyn crate::callrecord::CallRecordHook>> {
        self.addons
            .iter()
            .filter(|a| self.is_enabled(a.id(), config))
            .filter_map(|a| a.call_record_hook(db))
            .collect()
    }

    pub fn get_addon(&self, id: &str) -> Option<&dyn Addon> {
        self.addons
            .iter()
            .find(|a| a.id() == id)
            .map(|a| a.as_ref())
    }

    pub async fn authenticate_all(
        &self,
        state: AppState,
        identifier: &str,
        password: &str,
    ) -> anyhow::Result<Option<crate::models::user::Model>> {
        let config = state.config();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Some(user) = addon
                .authenticate(state.clone(), identifier, password)
                .await?
            {
                return Ok(Some(user));
            }
        }
        Ok(None)
    }

    pub fn apply_proxy_server_hooks(
        &self,
        mut builder: crate::proxy::server::SipServerBuilder,
        ctx: Arc<crate::app::CoreContext>,
    ) -> crate::proxy::server::SipServerBuilder {
        let config = &ctx.config;
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            builder = addon.proxy_server_hook(builder, ctx.clone());
            if let Some(entry) = addon.dialplan_inspector_entry(config) {
                builder = builder.with_dialplan_inspector_entry(entry);
            }
            if let Some(enricher) = addon.queue_location_enricher() {
                builder = builder.with_queue_location_enricher(enricher);
            }
        }
        builder
    }

    // ── Extension lifecycle hooks ───────────────────────────────────────────

    pub async fn on_extension_created(
        &self,
        config: &crate::config::Config,
        db: &sea_orm::DatabaseConnection,
        extension: &str,
    ) {
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Err(e) = addon.on_extension_created(db, extension).await {
                tracing::warn!(
                    addon = %addon.id(),
                    extension = %extension,
                    error = %e,
                    "on_extension_created hook failed"
                );
            }
        }
    }

    pub async fn on_extension_updated(
        &self,
        config: &crate::config::Config,
        db: &sea_orm::DatabaseConnection,
        extension: &str,
    ) {
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Err(e) = addon.on_extension_updated(db, extension).await {
                tracing::warn!(
                    addon = %addon.id(),
                    extension = %extension,
                    error = %e,
                    "on_extension_updated hook failed"
                );
            }
        }
    }

    pub async fn on_extension_deleting(
        &self,
        config: &crate::config::Config,
        db: &sea_orm::DatabaseConnection,
        extension: &str,
    ) {
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            if let Err(e) = addon.on_extension_deleting(db, extension).await {
                tracing::warn!(
                    addon = %addon.id(),
                    extension = %extension,
                    error = %e,
                    "on_extension_deleting hook failed"
                );
            }
        }
    }

    /// Build a call app from the first enabled addon that handles the app name.
    pub async fn build_call_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        context: &crate::call::app::ApplicationContext,
    ) -> Option<Box<dyn crate::call::app::CallApp>> {
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), &context.config) {
                continue;
            }
            if let Some(app) = addon
                .build_call_app(app_name, params.clone(), context)
                .await
            {
                return Some(app);
            }
        }
        None
    }

    /// Shutdown all addons, releasing resources.
    pub async fn shutdown_all(&self) {
        for addon in &self.addons {
            addon.shutdown().await;
        }
    }

    pub fn branding_provider(&self) -> Option<Arc<dyn crate::branding::BrandingProvider>> {
        self.addons.iter().find_map(|a| a.branding())
    }

    /// Dispatch one authentication attempt to every addon.
    pub fn dispatch_auth_attempt(&self, attempt: &crate::addons::events::AuthAttempt) {
        for addon in &self.addons {
            addon.on_auth_attempt(attempt);
        }
    }

    /// Run database migrations for all enabled addons.
    pub async fn run_migrations(&self, db: &sea_orm::DatabaseConnection) -> anyhow::Result<()> {
        for addon in &self.addons {
            let migrations = addon.migrations();
            if migrations.is_empty() {
                continue;
            }
            run_tracked_migrations(db, addon.id(), addon.name(), migrations).await?;
        }
        Ok(())
    }
}

/// Apply an addon's migrations once, tracked in a per-addon table
/// (`seaql_migrations_<addon_id>`) so subsequent boots skip them.
/// Column layout matches sea-orm's own `seaql_migrations` convention.
pub(crate) async fn run_tracked_migrations(
    db: &sea_orm::DatabaseConnection,
    addon_id: &str,
    addon_name: &str,
    migrations: Vec<Box<dyn sea_orm_migration::MigrationTrait>>,
) -> anyhow::Result<()> {
    use sea_orm::{ConnectionTrait, Statement};
    use chrono::Utc;

    let manager = sea_orm_migration::SchemaManager::new(db);
    let tracking_table = format!("seaql_migrations_{}", addon_id);

    if !manager
        .has_table(tracking_table.as_str())
        .await
        .unwrap_or(false)
    {
        db.execute_unprepared(&format!(
            "CREATE TABLE {} (version VARCHAR(255) PRIMARY KEY, applied_at BIGINT)",
            tracking_table
        ))
        .await?;
    }

    let backend = db.get_database_backend();
    let mut applied = std::collections::HashSet::new();
    let rows = db
        .query_all_raw(Statement::from_string(
            backend.clone(),
            format!("SELECT version FROM {}", tracking_table),
        ))
        .await?;
    for row in rows {
        if let Ok(v) = row.try_get_by_index::<String>(0) {
            applied.insert(v);
        }
    }

    for migration in migrations {
        let name = migration.name().to_string();
        if !name.starts_with(addon_id) {
            tracing::warn!(
                addon = addon_id,
                migration = %name,
                "migration name does not start with its addon id; expected prefix '{}_'",
                addon_id
            );
        }
        if applied.contains(&name) {
            continue;
        }
        if let Err(e) = migration.up(&manager).await {
            return Err(anyhow::anyhow!(
                "Migration '{}' for addon '{}' failed: {}",
                name,
                addon_name,
                e
            ));
        }
        let mut insert = sea_orm::sea_query::Query::insert();
        insert
            .into_table(sea_orm::sea_query::Alias::new(tracking_table.as_str()))
            .columns([
                sea_orm::sea_query::Alias::new("version"),
                sea_orm::sea_query::Alias::new("applied_at"),
            ])
            .values_panic([name.clone().into(), Utc::now().timestamp().into()]);
        db.execute(&insert)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to record migration '{}' for addon '{}': {}",
                    name,
                    addon_name,
                    e
                )
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    use super::run_tracked_migrations;
    use sea_orm::Statement;
    use sea_orm_migration::prelude::*;

    struct FixedMigration {
        name: &'static str,
        sql: &'static str,
    }

    impl MigrationName for FixedMigration {
        fn name(&self) -> &str {
            self.name
        }
    }

    #[async_trait::async_trait]
    impl MigrationTrait for FixedMigration {
        async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager
                .get_connection()
                .execute_unprepared(self.sql)
                .await?;
            Ok(())
        }
    }

    fn one_migration(name: &'static str, sql: &'static str) -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(FixedMigration { name, sql })]
    }

    #[tokio::test]
    async fn tracked_migrations_apply_exactly_once() {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let sql = "CREATE TABLE t_a (id INTEGER PRIMARY KEY)";

        run_tracked_migrations(&db, "testaddon", "TestAddon", one_migration("testaddon_m0001_t", sql))
            .await
            .unwrap();
        run_tracked_migrations(&db, "testaddon", "TestAddon", one_migration("testaddon_m0001_t", sql))
            .await
            .unwrap();

        let backend = db.get_database_backend();
        let rows = db
            .query_all_raw(Statement::from_string(
                backend.clone(),
                "SELECT version FROM seaql_migrations_testaddon",
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].try_get_by_index::<String>(0).unwrap(),
            "testaddon_m0001_t"
        );
    }

    #[tokio::test]
    async fn unprefixed_migration_applies_with_warning() {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();

        run_tracked_migrations(
            &db,
            "testaddon",
            "TestAddon",
            one_migration("legacy_name", "CREATE TABLE t_b (id INTEGER PRIMARY KEY)"),
        )
        .await
        .unwrap();

        let backend = db.get_database_backend();
        let rows = db
            .query_all_raw(Statement::from_string(
                backend.clone(),
                "SELECT name FROM sqlite_master WHERE type='table' AND name='t_b'",
            ))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn tracking_survives_fresh_connection_against_file_db() {
        let dir = std::env::temp_dir().join(format!(
            "rustpbx_mig_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}/tracked.db?mode=rwc", dir.display());

        let db = sea_orm::Database::connect(&url).await.unwrap();
        run_tracked_migrations(&db, "testaddon", "TestAddon", one_migration("testaddon_m0001_t", "CREATE TABLE t_c (id INTEGER PRIMARY KEY)"))
            .await
            .unwrap();
        db.close().await.unwrap();

        let db2 = sea_orm::Database::connect(&url).await.unwrap();
        run_tracked_migrations(&db2, "testaddon", "TestAddon", one_migration("testaddon_m0001_t", "CREATE TABLE t_c (id INTEGER PRIMARY KEY)"))
            .await
            .unwrap();
        db2.close().await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod asset_path_tests {
    use super::AddonRegistry;
    use crate::addons::Addon;
    use crate::config::Config;
    use std::sync::Arc;

    struct PathAddon;

    #[async_trait::async_trait]
    impl Addon for PathAddon {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn id(&self) -> &'static str {
            "pathaddon"
        }
        fn name(&self) -> &'static str {
            "PathAddon"
        }
        fn router(&self, _state: crate::app::AppState) -> Option<axum::Router> {
            None
        }
        async fn initialize(&self, _state: crate::app::AppState) -> anyhow::Result<()> {
            Ok(())
        }
        fn template_dir(&self) -> Option<String> {
            Some("/opt/pathaddon/templates".into())
        }
        fn static_dir(&self) -> Option<String> {
            Some("/opt/pathaddon/static".into())
        }
    }

    fn enabled_config() -> Config {
        let mut config = Config::default();
        config.proxy.addons = Some(vec!["pathaddon".into()]);
        config
    }

    #[test]
    fn template_dir_takes_priority_over_conventions() {
        let registry = AddonRegistry::with_extra_addons(vec![Arc::new(PathAddon)]);
        let dirs = registry.get_template_dirs(&enabled_config());
        assert_eq!(
            dirs.first().map(String::as_str),
            Some("/opt/pathaddon/templates")
        );
        assert!(dirs.contains(&"src/addons/pathaddon/templates".to_string()));
        assert!(dirs.contains(&"templates/pathaddon".to_string()));
    }

    #[test]
    fn static_mounts_cover_declaring_enabled_addons_only() {
        let registry = AddonRegistry::with_extra_addons(vec![Arc::new(PathAddon)]);
        let mounts = registry.get_static_mounts(&enabled_config());
        assert_eq!(
            mounts,
            vec![(
                "pathaddon".to_string(),
                "/opt/pathaddon/static".to_string()
            )]
        );

        let mut other_enabled = Config::default();
        other_enabled.proxy.addons = Some(vec!["other".into()]);
        assert!(registry.get_static_mounts(&other_enabled).is_empty());
        assert!(registry.get_static_mounts(&Config::default()).is_empty());
    }

    struct CommercialAddon;

    #[async_trait::async_trait]
    impl Addon for CommercialAddon {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn id(&self) -> &'static str {
            "commercialaddon"
        }
        fn name(&self) -> &'static str {
            "CommercialAddon"
        }
        fn category(&self) -> crate::addons::AddonCategory {
            crate::addons::AddonCategory::Commercial
        }
        fn router(&self, _state: crate::app::AppState) -> Option<axum::Router> {
            None
        }
        async fn initialize(&self, _state: crate::app::AppState) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn has_commercial_reflects_registered_addon_categories() {
        assert!(!AddonRegistry::new().has_commercial());
        assert!(
            !AddonRegistry::with_extra_addons(vec![Arc::new(PathAddon)]).has_commercial()
        );
        assert!(AddonRegistry::with_extra_addons(vec![Arc::new(
            PathAddon
        ), Arc::new(CommercialAddon)])
        .has_commercial());
    }

    #[test]
    fn license_status_gates_enabled_addons() {
        use crate::license::{LicenseStatus, record_startup_results};
        use std::collections::HashMap;

        let registry = AddonRegistry::with_extra_addons(vec![Arc::new(PathAddon)]);
        let config = enabled_config();

        record_startup_results(HashMap::new());
        assert!(registry.is_enabled("pathaddon", &config));

        let mut denied = HashMap::new();
        denied.insert(
            "pathaddon".to_string(),
            LicenseStatus {
                key_name: "k".into(),
                valid: false,
                expired: false,
                expiry: None,
                plan: String::new(),
                is_trial: false,
                scope: None,
            },
        );
        record_startup_results(denied);
        assert!(!registry.is_enabled("pathaddon", &config));

        let mut trial = HashMap::new();
        trial.insert(
            "pathaddon".to_string(),
            LicenseStatus {
                key_name: "trial".into(),
                valid: true,
                expired: false,
                expiry: None,
                plan: "trial".into(),
                is_trial: true,
                scope: None,
            },
        );
        record_startup_results(trial);
        assert!(registry.is_enabled("pathaddon", &config));

        record_startup_results(HashMap::new());
    }
}
