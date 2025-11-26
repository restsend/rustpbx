use crate::app::AppState;
use crate::addons::Addon;

pub struct AddonRegistry {
    addons: Vec<Box<dyn Addon>>,
}

impl AddonRegistry {
    pub fn new() -> Self {
        let mut addons: Vec<Box<dyn Addon>> = Vec::new();

        // ACME Addon (Free/Built-in for now as per request)
        #[cfg(feature = "addon-acme")]
        addons.push(Box::new(super::acme::AcmeAddon::new()));

        Self { addons }
    }

    pub async fn initialize_all(&self, state: AppState) -> anyhow::Result<()> {
        for addon in &self.addons {
            tracing::info!("Initializing addon: {}", addon.name());
            // Commercial addons would check license here
            if let Err(e) = addon.initialize(state.clone()).await {
                tracing::error!("Failed to initialize addon {}: {}", addon.name(), e);
            }
        }
        Ok(())
    }

    pub fn get_routers(&self, state: AppState) -> axum::Router {
        let mut router = axum::Router::new();
        for addon in &self.addons {
            if let Some(r) = addon.router(state.clone()) {
                router = router.merge(r);
            }
        }
        router
    }

    pub fn get_sidebar_items(&self) -> Vec<super::SidebarItem> {
        self.addons.iter().flat_map(|a| a.sidebar_items()).collect()
    }

    pub fn get_template_dirs(&self) -> Vec<String> {
        self.addons.iter().filter_map(|a| a.template_dir()).collect()
    }

    pub fn list_addons(&self) -> Vec<super::AddonInfo> {
        self.addons.iter().map(|a| super::AddonInfo {
            id: a.id().to_string(),
            name: a.name().to_string(),
            description: a.description().to_string(),
            enabled: true, // Currently all loaded addons are enabled
        }).collect()
    }
}
