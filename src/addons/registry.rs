use crate::addons::Addon;
use crate::app::AppState;
use std::sync::Arc;

pub struct AddonRegistry {
    addons: Vec<Box<dyn Addon>>,
}

impl AddonRegistry {
    pub fn new() -> Self {
        let mut addons: Vec<Box<dyn Addon>> = Vec::new();

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

        // Queue Addon
        addons.push(Box::new(super::queue::QueueAddon::new()));

        Self { addons }
    }

    pub async fn initialize_all(&self, state: AppState) -> anyhow::Result<()> {
        let config = state.config();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), &config) {
                tracing::info!("Addon {} is disabled", addon.name());
                continue;
            }
            tracing::info!("Initializing addon: {}", addon.name());
            // Commercial addons would check license here
            if let Err(e) = addon.initialize(state.clone()).await {
                tracing::error!("Failed to initialize addon {}: {}", addon.name(), e);
            }
        }
        Ok(())
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
                [
                    format!("src/addons/{}/templates", a.id()),
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
