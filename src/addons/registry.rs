use crate::addons::Addon;
use crate::app::AppState;

pub struct AddonRegistry {
    addons: Vec<Box<dyn Addon>>,
}

impl AddonRegistry {
    pub fn new() -> Self {
        let mut addons: Vec<Box<dyn Addon>> = Vec::new();

        // ACME Addon (Free/Built-in for now as per request)
        #[cfg(feature = "addon-acme")]
        addons.push(Box::new(super::acme::AcmeAddon::new()));

        // Wholesale Addon
        #[cfg(feature = "addon-wholesale")]
        addons.push(Box::new(super::wholesale::WholesaleAddon::new()));

        Self { addons }
    }

    pub async fn initialize_all(&self, state: AppState) -> anyhow::Result<()> {
        let config = &state.config;
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

    pub fn get_routers(&self, state: AppState) -> axum::Router {
        let config = &state.config;
        let mut router = axum::Router::new();
        for addon in &self.addons {
            if !self.is_enabled(addon.id(), &config) {
                continue;
            }
            if let Some(r) = addon.router(state.clone()) {
                router = router.merge(r);
            }
        }
        router
    }

    pub fn get_sidebar_items(&self, state: AppState) -> Vec<super::SidebarItem> {
        let config = &state.config;
        self.addons
            .iter()
            .filter(|a| self.is_enabled(a.id(), &config))
            .flat_map(|a| a.sidebar_items())
            .collect()
    }

    pub fn get_template_dirs(&self, state: AppState) -> Vec<String> {
        let config = &state.config;
        self.addons
            .iter()
            .filter(|a| self.is_enabled(a.id(), &config))
            .flat_map(|a| {
                [
                    a.template_dir(),
                    format!("src/{}", a.template_dir()),
                    format!("templates/{}", a.template_dir()),
                ]
            })
            .collect()
    }

    pub fn list_addons(&self) -> Vec<super::AddonInfo> {
        self.addons
            .iter()
            .map(|a| {
                let sidebar = a.sidebar_items();
                let config_url = sidebar.first().map(|s| s.url.clone());

                super::AddonInfo {
                    id: a.id().to_string(),
                    name: a.name().to_string(),
                    description: a.description().to_string(),
                    enabled: true, // Currently all loaded addons are enabled
                    config_url,
                }
            })
            .collect()
    }

    pub fn is_enabled(&self, id: &str, config: &crate::config::Config) -> bool {
        if let Some(proxy) = &config.proxy {
            if let Some(addons) = &proxy.addons {
                return addons.iter().any(|a| a == id);
            }
        }
        false
    }
}
