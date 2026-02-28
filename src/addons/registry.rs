use crate::addons::Addon;
use crate::app::AppState;
use std::sync::Arc;

pub struct AddonRegistry {
    addons: Vec<Box<dyn Addon>>,
}

impl AddonRegistry {
    pub fn new() -> Self {
        let mut addons: Vec<Box<dyn Addon>> = Vec::new();

        #[cfg(all(feature = "addon-observability", not(feature = "addon-telemetry")))]
        {
            if let Err(e) = super::observability::ObservabilityAddon::install_recorder() {
                tracing::warn!("Prometheus recorder not installed: {}", e);
            }
            addons.push(Box::new(super::observability::ObservabilityAddon::new()));
        }

        #[cfg(feature = "addon-telemetry")]
        {
            if let Err(e) = crate::addons::telemetry::TelemetryAddon::install_recorder() {
                tracing::warn!("Prometheus recorder not installed (commercial): {}", e);
            }
            addons.push(Box::new(crate::addons::telemetry::TelemetryAddon::new()));
        }

        // ACME Addon (Free/Built-in for now as per request)
        #[cfg(feature = "addon-acme")]
        addons.push(Box::new(super::acme::AcmeAddon::new()));

        // Archive Addon
        #[cfg(feature = "addon-archive")]
        addons.push(Box::new(super::archive::ArchiveAddon::new()));

        // Wholesale Addon
        #[cfg(feature = "addon-wholesale")]
        addons.push(Box::new(super::wholesale::WholesaleAddon::new()));

        // Transcript Addon
        #[cfg(feature = "addon-transcript")]
        addons.push(Box::new(super::transcript::TranscriptAddon::new()));

        // Endpoint Manager Addon
        #[cfg(feature = "addon-endpoint-manager")]
        addons.push(Box::new(
            super::endpoint_manager::EndpointManagerAddon::new(),
        ));

        // Enterprise Auth Addon
        #[cfg(feature = "addon-enterprise-auth")]
        addons.push(Box::new(super::enterprise_auth::EnterpriseAuthAddon::new()));

        // Voicemail Addon
        #[cfg(feature = "addon-voicemail")]
        addons.push(Box::new(crate::voicemail::VoicemailAddon::new()));

        // Queue Addon
        addons.push(Box::new(super::queue::QueueAddon::new()));

        Self { addons }
    }

    pub async fn initialize_all(&self, state: AppState) -> anyhow::Result<()> {
        let config = state.config();

        // Tell the license module where the storage directory lives so the
        // free-trial flag file lands in the right place.
        crate::license::set_storage_dir(&config.storage_dir);

        // Check licenses for all commercial addons at startup
        #[cfg(feature = "commerce")]
        {
            self.check_all_licenses(config).await;
        }

        for addon in &self.addons {
            if !self.is_enabled(addon.id(), &config) {
                tracing::info!("Addon {} is disabled", addon.name());
                continue;
            }

            // Check license for commercial addons
            #[cfg(feature = "commerce")]
            {
                if addon.category() == crate::addons::AddonCategory::Commercial {
                    let can_run =
                        crate::license::can_enable_addon(addon.id(), true, &config.licenses).await;
                    if !can_run {
                        tracing::error!(
                            "Cannot enable addon {}: license is invalid or expired",
                            addon.name()
                        );
                        continue;
                    }
                }
            }

            tracing::info!("Initializing addon: {}", addon.name());
            if let Err(e) = addon.initialize(state.clone()).await {
                tracing::error!("Failed to initialize addon {}: {}", addon.name(), e);
            }
        }
        Ok(())
    }

    /// Check all licenses at startup and log any issues.
    #[cfg(feature = "commerce")]
    async fn check_all_licenses(&self, config: &crate::config::Config) {
        let commercial_addons: Vec<String> = self
            .addons
            .iter()
            .filter(|a| a.category() == crate::addons::AddonCategory::Commercial)
            .filter(|a| self.is_enabled(a.id(), config))
            .map(|a| a.id().to_string())
            .collect();

        if commercial_addons.is_empty() {
            return;
        }

        let results =
            crate::license::check_all_addon_licenses(&commercial_addons, &config.licenses).await;

        // Populate the in-process cache so runtime checks never hit the network.
        crate::license::record_startup_results(results.clone());

        for (addon_id, status) in results {
            if status.is_trial && status.valid {
                tracing::info!(
                    "Addon '{}' running under free-trial ({})",
                    addon_id,
                    status.plan
                );
            } else if !status.valid {
                if status.is_trial {
                    tracing::error!(
                        "Addon '{}' free-trial has expired; a valid license key is required",
                        addon_id
                    );
                } else if status.key_name.is_empty() {
                    tracing::error!(
                        "Addon '{}' is commercial but no license key is configured",
                        addon_id
                    );
                } else if status.expired {
                    tracing::error!(
                        "Addon '{}' license has expired (key: {})",
                        addon_id,
                        status.key_name
                    );
                } else {
                    tracing::error!(
                        "Addon '{}' license is invalid (key: {})",
                        addon_id,
                        status.key_name
                    );
                }
            } else {
                tracing::info!(
                    "Addon '{}' license is valid (key: {}, expires: {:?})",
                    addon_id,
                    status.key_name,
                    status.expiry
                );
            }
        }
    }

    pub async fn seed_all_fixtures(&self, state: AppState) -> anyhow::Result<()> {
        let config = state.config();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), &config) {
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

    pub fn get_injected_scripts(&self, path: &str, config: &crate::config::Config) -> Vec<String> {
        let mut scripts = Vec::new();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), config) {
                continue;
            }
            for injection in addon.inject_scripts() {
                if let Ok(re) = regex::Regex::new(injection.url_path_regex) {
                    if re.is_match(path) {
                        scripts.push(injection.script_url);
                    }
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

    pub fn get_template_dirs(&self, state: AppState) -> Vec<String> {
        let config = state.config();
        self.addons
            .iter()
            .filter(|a| self.is_enabled(a.id(), config))
            .flat_map(|a| {
                vec![
                    format!("src/addons/{}/templates", a.id()),
                    format!("src/{}/templates", a.id()),
                    format!("templates/{}", a.id()),
                ]
            })
            .collect()
    }

    pub fn list_addons(&self, state: AppState) -> Vec<super::AddonInfo> {
        self.addons
            .iter()
            .map(|a| {
                let config_url = a.config_url(state.clone());

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
                    screenshots: a.screenshots().iter().map(|s| s.to_string()).collect(),
                    restart_required: false, // Caller should set this
                    license_status: None,
                    license_expiry: None,
                    license_plan: None,
                }
            })
            .collect()
    }

    pub fn is_enabled(&self, id: &str, config: &crate::config::Config) -> bool {
        if let Some(addons) = &config.proxy.addons {
            return addons.iter().any(|a| a == id);
        }
        false
    }

    pub fn get_call_record_hooks(
        &self,
        config: &crate::config::Config,
        db: &sea_orm::DatabaseConnection,
    ) -> Vec<Box<dyn crate::callrecord::CallRecordHook>> {
        self.addons
            .iter()
            .filter(|a| self.is_enabled(a.id(), &config))
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
            if !self.is_enabled(addon.id(), &config) {
                continue;
            }
            builder = addon.proxy_server_hook(builder, ctx.clone());
        }
        builder
    }
}
