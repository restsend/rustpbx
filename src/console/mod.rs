use crate::config::ConsoleConfig;
use crate::console::middleware::RenderTemplate;
use crate::proxy::server::SipServerRef;
use crate::{app::AppStateInner, callrecord::CallRecordFormatter};
use anyhow::Result;
use axum::response::{IntoResponse, Response};
use minijinja::Environment;
use sea_orm::DatabaseConnection;
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock, Weak};

pub mod auth;
pub mod handlers;
pub mod middleware;
pub use handlers::router;

#[derive(Clone)]
pub struct ConsoleState {
    db: DatabaseConnection,
    config: ConsoleConfig,
    session_key: Vec<u8>,
    sip_server: Arc<RwLock<Option<SipServerRef>>>,
    app_state: Arc<RwLock<Option<Weak<AppStateInner>>>>,
    callrecord_formatter: Arc<dyn CallRecordFormatter>,
}

impl ConsoleState {
    pub async fn initialize(
        callrecord_formatter: Arc<dyn CallRecordFormatter>,
        db: DatabaseConnection,
        config: ConsoleConfig,
    ) -> Result<Arc<Self>> {
        let key_material: [u8; 32] = Sha256::digest(config.session_secret.as_bytes()).into();
        let session_key = key_material.to_vec();
        let mut config = config;
        config.base_path = normalize_base_path(&config.base_path);
        Ok(Arc::new(Self {
            db,
            config,
            session_key,
            sip_server: Arc::new(RwLock::new(None)),
            app_state: Arc::new(RwLock::new(None)),
            callrecord_formatter,
        }))
    }

    pub fn render(&self, template: &str, ctx: serde_json::Value) -> Response {
        let mut ctx = ctx;
        if ctx.is_object() {
            if let Some(map) = ctx.as_object_mut() {
                map.entry("base_path")
                    .or_insert_with(|| serde_json::Value::String(self.base_path().to_string()));

                // Inject addon sidebar items
                if let Some(app_state) = self.app_state() {
                    let addon_items = app_state
                        .addon_registry
                        .get_sidebar_items(app_state.clone());
                    map.entry("addon_sidebar_items").or_insert_with(|| {
                        serde_json::to_value(addon_items).unwrap_or(serde_json::Value::Null)
                    });
                }
                map.entry("logout_url")
                    .or_insert_with(|| serde_json::Value::String(self.url_for("/logout")));
                map.entry("forgot_url")
                    .or_insert_with(|| serde_json::Value::String(self.forgot_url()));
                map.entry("login_url")
                    .or_insert_with(|| serde_json::Value::String(self.url_for("/login")));
                map.entry("register_url")
                    .or_insert_with(|| serde_json::Value::String(self.register_url(None)));
                map.entry("username").or_insert(serde_json::Value::Null);
                map.entry("email").or_insert(serde_json::Value::Null);
                map.entry("site_version").or_insert_with(|| {
                    serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string())
                });
                map.entry("edition").or_insert_with(|| {
                    if cfg!(feature = "commerce") {
                        serde_json::Value::String("commerce".to_string())
                    } else {
                        serde_json::Value::String("community".to_string())
                    }
                });
                map.entry("site_name")
                    .or_insert_with(|| serde_json::Value::String("RustPBX".to_string()));
                map.entry("page_title")
                    .or_insert_with(|| serde_json::Value::String("RustPBX admin".to_string()));
                map.entry("site_description").or_insert_with(|| {
                    serde_json::Value::String("RustPBX - A Rust-based PBX system".to_string())
                });
                map.entry("site_url").or_insert_with(|| {
                    serde_json::Value::String("https://rustpbx.com".to_string())
                });
                map.entry("site_footer").or_insert_with(|| {
                    serde_json::Value::String("© 2025 RustPBX. All rights reserved.".to_string())
                });
                map.entry("site_logo").or_insert_with(|| {
                    serde_json::Value::String("/static/images/logo.png".to_string())
                });
                map.entry("site_logo_mini").or_insert_with(|| {
                    serde_json::Value::String("/static/images/logo-mini.png".to_string())
                });
                map.entry("favicon_url").or_insert_with(|| {
                    serde_json::Value::String("/static/images/favicon.png".to_string())
                });
                map.entry("demo_mode")
                    .or_insert_with(|| serde_json::Value::Bool(self.config().demo_mode));
                if let Some(ref alpine_js) = self.config.alpine_js {
                    map.entry("alpine_js")
                        .or_insert_with(|| serde_json::Value::String(alpine_js.clone()));
                }
                if let Some(ref tailwind_js) = self.config.tailwind_js {
                    map.entry("tailwind_js")
                        .or_insert_with(|| serde_json::Value::String(tailwind_js.clone()));
                }
                if let Some(ref chart_js) = self.config.chart_js {
                    map.entry("chart_js")
                        .or_insert_with(|| serde_json::Value::String(chart_js.clone()));
                }
            }
        }

        let mut tmpl_env = Environment::new();

        tmpl_env.add_filter(
            "format",
            |format_str: &str, value: minijinja::Value| -> Result<String, minijinja::Error> {
                if let Ok(num) = f64::try_from(value.clone()) {
                    if num == 0.0 {
                        return Ok("0".to_string());
                    }
                    match format_str {
                        "%.1f" => Ok(format!("{:.1}", num)),
                        "%.2f" => Ok(format!("{:.2}", num)),
                        "%.3f" => Ok(format!("{:.3}", num)),
                        "%.4f" => Ok(format!("{:.4}", num)),
                        "%.5f" => Ok(format!("{:.5}", num)),
                        _ => Ok(format!("{}", num)),
                    }
                } else {
                    Ok(value.to_string())
                }
            },
        );

        tmpl_env.add_filter(
            "json",
            |value: minijinja::Value| -> Result<String, minijinja::Error> {
                serde_json::to_string(&value).map_err(|e| {
                    minijinja::Error::new(
                        minijinja::ErrorKind::InvalidOperation,
                        format!("failed to serialize to json: {}", e),
                    )
                })
            },
        );

        let mut paths = vec!["templates".to_string()];
        if let Some(app_state) = self.app_state() {
            paths.extend(
                app_state
                    .addon_registry
                    .get_template_dirs(app_state.clone()),
            );
        }

        tmpl_env.set_loader(move |name| {
            for base in &paths {
                let path = std::path::Path::new(base).join(name);
                if path.exists() {
                    return std::fs::read_to_string(path).map(Some).map_err(|_| {
                        minijinja::Error::new(
                            minijinja::ErrorKind::TemplateNotFound,
                            "failed to load template",
                        )
                    });
                }
            }
            Ok(None)
        });

        RenderTemplate {
            tmpl_env: &tmpl_env,
            template_name: template,
            context: &ctx,
        }
        .into_response()
    }

    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    pub fn set_sip_server(&self, server: Option<SipServerRef>) {
        if let Ok(mut slot) = self.sip_server.write() {
            *slot = server;
        }
    }

    pub fn sip_server(&self) -> Option<SipServerRef> {
        self.sip_server.read().ok().and_then(|guard| guard.clone())
    }

    pub fn set_app_state(&self, app_state: Option<Weak<AppStateInner>>) {
        if let Ok(mut slot) = self.app_state.write() {
            *slot = app_state;
        }
    }

    pub fn app_state(&self) -> Option<Arc<AppStateInner>> {
        self.app_state
            .read()
            .ok()
            .and_then(|opt| opt.as_ref().and_then(|weak| weak.upgrade()))
    }

    pub fn config(&self) -> Arc<crate::config::Config> {
        self.app_state()
            .map(|s| s.config().clone())
            .unwrap_or_else(|| Arc::new(crate::config::Config::default()))
    }

    pub fn get_injected_scripts(&self, path: &str) -> Option<Vec<String>> {
        self.app_state().map(|app_state| {
            app_state
                .addon_registry
                .get_injected_scripts(path, app_state.config())
        })
    }

    pub fn url_for(&self, suffix: &str) -> String {
        let trimmed = suffix.trim();
        if trimmed.is_empty() || trimmed == "/" {
            return format!("{}/", self.base_path());
        }
        if trimmed.starts_with('/') {
            if self.base_path() == "/" {
                trimmed.to_string()
            } else {
                format!("{}{}", self.base_path(), trimmed)
            }
        } else if self.base_path() == "/" {
            format!("/{}", trimmed)
        } else {
            format!("{}/{}", self.base_path(), trimmed)
        }
    }

    pub fn base_path(&self) -> &str {
        &self.config.base_path
    }

    pub fn registration_allowed_by_config(&self) -> bool {
        self.config.allow_registration
    }

    pub fn login_url(&self, next: Option<String>) -> String {
        let mut url = self.url_for("/login");
        if let Some(next) = next {
            url.push_str(&format!("?next={}", next));
        }
        url
    }

    pub fn register_url(&self, next: Option<String>) -> String {
        let mut url = self.url_for("/register");
        if let Some(next) = next {
            url.push_str(&format!("?next={}", next));
        }
        url
    }

    pub fn forgot_url(&self) -> String {
        self.url_for("/forgot")
    }

    pub fn get_sip_server(&self) -> Option<SipServerRef> {
        self.sip_server.read().unwrap().clone()
    }
}

fn normalize_base_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/console".to_string();
    }
    let mut normalized = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{}", trimmed)
    };
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    if normalized.is_empty() {
        "/console".to_string()
    } else {
        normalized
    }
}
