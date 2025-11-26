# RustPBX Addons Architecture Scheme

This document details the plugin (Addons) architecture scheme for RustPBX. This scheme aims to support modular development, allowing for the flexible integration of free and commercial features via `git submodule` and `cargo features`.

## 1. Architecture Overview

The Addons system consists of the following core components:

1.  **Addon Trait**: Defines the interface that all plugins must implement (lifecycle management, route injection, UI injection).
2.  **Addon Manager**: Responsible for loading and initializing plugins at application startup, and aggregating routes and UI elements.
3.  **Feature Flags**: Uses `Cargo.toml` features to control the compilation and enabling of plugins.
4.  **License System**: Commercial plugins check for authorization at runtime.

### Directory Structure

```
src/
  addons/
    mod.rs          # Addon Trait definition and Manager implementation
    registry.rs     # Plugin registry
    acme/           # [Built-in] ACME certificate management plugin (Free)
    voicemail/      # [Submodule] Voicemail plugin (Commercial)
    wholesale/      # [Submodule] Line management plugin (Commercial)
    backup/         # [Submodule] Auto-backup plugin (Commercial)
    demo/           # Example plugin (Free)
```

## 2. Addon Trait Definition

The `Addon` trait is defined in `src/addons/mod.rs`. All plugins must implement this trait.

```rust
use async_trait::async_trait;
use axum::Router;
use crate::app::AppState;
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarItem {
    pub name: String,
    pub icon: String, // FontAwesome class or SVG
    pub url: String,
    pub permission: Option<String>, // Required permission
}

#[async_trait]
pub trait Addon: Send + Sync {
    /// Plugin unique identifier
    fn id(&self) -> &'static str;
    
    /// Plugin display name
    fn name(&self) -> &'static str;

    /// Initialize plugin (database migration, background task startup, etc.)
    async fn initialize(&self, state: AppState) -> anyhow::Result<()>;

    /// Return plugin API and UI routes
    /// These routes will be merged into the main application's Router
    fn router(&self, state: AppState) -> Option<Router>;

    /// Return Sidebar menu items
    fn sidebar_items(&self) -> Vec<SidebarItem> {
        vec![]
    }

    /// Return Settings page injection items (HTML fragments or configuration definitions)
    fn settings_items(&self) -> Option<String> {
        None
    }
}
```

## 3. Plugin Registration and Loading (Registry)

Plugin registration logic is implemented in `src/addons/registry.rs`. The `cfg` macro is used to decide whether to load a specific plugin based on feature flags.

```rust
// src/addons/registry.rs
use crate::app::AppState;
use super::Addon;
use std::sync::Arc;

pub struct AddonRegistry {
    addons: Vec<Box<dyn Addon>>,
}

impl AddonRegistry {
    pub fn new() -> Self {
        let mut addons: Vec<Box<dyn Addon>> = Vec::new();

        // Example plugin
        #[cfg(feature = "addon-acme")]
        addons.push(Box::new(super::acme::AcmeAddon::new()));

        // Commercial plugin - Voicemail
        #[cfg(feature = "addon-voicemail")]
        addons.push(Box::new(super::voicemail::VoicemailAddon::new()));

        // Commercial plugin - Wholesale
        #[cfg(feature = "addon-wholesale")]
        addons.push(Box::new(super::wholesale::WholesaleAddon::new()));
        
        // Commercial plugin - Backup
        #[cfg(feature = "addon-backup")]
        addons.push(Box::new(super::backup::BackupAddon::new()));

        Self { addons }
    }

    pub async fn initialize_all(&self, state: AppState) -> anyhow::Result<()> {
        for addon in &self.addons {
            tracing::info!("Initializing addon: {}", addon.name());
            // Commercial plugins perform License checks here
            if let Err(e) = addon.initialize(state.clone()).await {
                tracing::error!("Failed to initialize addon {}: {}", addon.name(), e);
                // Depending on policy, can choose to panic or skip
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
}
```

## 4. Integration into Main Application

### 4.1 Cargo.toml Configuration

```toml
[features]
# ... existing features ...
addon-acme = []
addon-demo = []
addon-voicemail = []
addon-wholesale = []
addon-backup = []
```

### 4.2 AppState Integration

In `src/app.rs`, `AppState` needs to hold a reference to `AddonRegistry` (or pre-computed UI data) so it can be used when rendering templates.

```rust
// src/app.rs

pub struct AppStateInner {
    // ... existing fields ...
    pub addon_registry: Arc<crate::addons::registry::AddonRegistry>,
}

// Initialize in build()
let addon_registry = Arc::new(crate::addons::registry::AddonRegistry::new());
addon_registry.initialize_all(app_state.clone()).await?;
```

### 4.3 Route Integration

In the `create_router` function in `src/app.rs`:

```rust
fn create_router(state: AppState) -> Router {
    let mut router = Router::new();
    // ... existing setup ...

    // Merge plugin routes
    router = router.merge(state.addon_registry.get_routers(state.clone()));
    
    // ...
}
```

### 4.4 UI Injection (Sidebar)

In the `render` method in `src/console/mod.rs`, inject sidebar items into the template context.

```rust
// src/console/mod.rs

pub fn render(&self, template: &str, ctx: serde_json::Value) -> Response {
    let mut ctx = ctx;
    if let Some(map) = ctx.as_object_mut() {
        // ... existing context ...
        
        // Get plugin Sidebar Items
        if let Some(app_state) = self.app_state() {
             let addon_items = app_state.addon_registry.get_sidebar_items();
             map.entry("addon_sidebar_items").or_insert(serde_json::to_value(addon_items).unwrap());
        }
    }
    // ...
}
```

Render in `templates/console/layout.html`:

```html
<!-- Sidebar -->
<ul class="nav">
    <!-- Core Items -->
    <li class="nav-item">...</li>

    <!-- Addon Items -->
    {% for item in addon_sidebar_items %}
    <li class="nav-item">
        <a class="nav-link" href="{{ item.url }}">
            <i class="{{ item.icon }}"></i>
            <span class="menu-title">{{ item.name }}</span>
        </a>
    </li>
    {% endfor %}
</ul>
```

## 5. Commercial Plugin Implementation Example (Voicemail)

### 5.1 Directory Structure (`src/addons/voicemail/`)

```
src/addons/voicemail/
  Cargo.toml        # Independent dependencies (optional)
  mod.rs            # Implements Addon Trait
  handlers.rs       # API handler functions
  service.rs        # Business logic
  templates/        # Plugin-specific templates
    voicemail_settings.html
```

### 5.2 Implementation Code (`mod.rs`)

```rust
use async_trait::async_trait;
use crate::addons::{Addon, SidebarItem};
use axum::{Router, routing::get};

pub struct VoicemailAddon;

impl VoicemailAddon {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl Addon for VoicemailAddon {
    fn id(&self) -> &'static str { "voicemail" }
    fn name(&self) -> &'static str { "Voicemail Pro" }

    async fn initialize(&self, state: AppState) -> anyhow::Result<()> {
        // 1. License Check
        if !crate::license::check_feature("voicemail") {
            return Err(anyhow::anyhow!("License invalid for Voicemail"));
        }
        
        // 2. Init DB or Background Tasks
        Ok(())
    }

    fn router(&self, state: AppState) -> Option<Router> {
        let r = Router::new()
            .route("/console/voicemail", get(handlers::ui_index))
            .route("/api/voicemail/config", get(handlers::get_config).post(handlers::update_config));
        Some(r)
    }

    fn sidebar_items(&self) -> Vec<SidebarItem> {
        vec![SidebarItem {
            name: "Voicemail".to_string(),
            icon: "mdi mdi-voicemail".to_string(),
            url: "/console/voicemail".to_string(),
            permission: None,
        }]
    }
}
```

## 6. License Check Scheme

Implement simple License checking in `src/license.rs` (new).

```rust
pub fn check_feature(feature_name: &str) -> bool {
    // 1. Read license file (e.g., license.key)
    // 2. Verify signature (use public key to verify license content)
    // 3. Check if feature_name is in the authorized list
    // 4. Check expiration time
    
    // Example:
    // let license = load_license();
    // license.features.contains(feature_name) && !license.is_expired()
    true // Temporarily return true for development
}
```

## 7. Implementation Steps

1.  **Infrastructure**: Create `src/addons/mod.rs` and `src/addons/registry.rs`.
2.  **App Integration**: Modify `src/app.rs` and `src/console/mod.rs` to support Addon Registry.
3.  **UI Adjustment**: Modify `layout.html` to support dynamic Sidebar.
4.  **Develop Plugins**:
    *   Create `src/addons/voicemail` (git submodule).
    *   Create `src/addons/wholesale` (git submodule).
    *   Create `src/addons/backup` (git submodule).
5.  **License**: Implement `src/license.rs`.

This scheme achieves decoupling of core logic and plugin logic, dynamic UI injection, and commercial authorization control.
