use crate::config::ConsoleConfig;
use crate::console::i18n::{I18n, LocaleConfig, LocaleInfo, detect_locale};
use crate::console::middleware::RenderTemplate;
use crate::models::rbac::{role_permission, user_role};
use crate::proxy::server::SipServerRef;
use crate::{app::AppStateInner, callrecord::CallRecordFormatter};
use anyhow::Result;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use minijinja::Environment;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Instant;

pub mod auth;
pub mod handlers;
pub mod i18n;
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
    i18n: Arc<I18n>,
    perm_cache: Arc<Mutex<HashMap<i64, (Instant, HashSet<String>)>>>,
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

        // Build LocaleConfig from ConsoleConfig
        let locale_config = LocaleConfig {
            default: config.locale_default.clone(),
            available: config
                .locales
                .iter()
                .map(|(code, info)| LocaleInfo {
                    code: code.clone(),
                    name: info.name.clone(),
                    native_name: info.native_name.clone(),
                })
                .collect(),
        };
        let i18n = Arc::new(I18n::new(locale_config));

        Ok(Arc::new(Self {
            db,
            config,
            session_key,
            sip_server: Arc::new(RwLock::new(None)),
            app_state: Arc::new(RwLock::new(None)),
            callrecord_formatter,
            i18n,
            perm_cache: Arc::new(Mutex::new(HashMap::new())),
        }))
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    /// Render a template using the default locale.
    ///
    /// Existing call-sites that don't need i18n can keep using this signature
    /// unchanged.  For locale-aware rendering use `render_with_headers` or
    /// `render_with_locale`.
    pub fn render(&self, template: &str, ctx: serde_json::Value) -> Response {
        let locale = self.i18n.default_locale().to_string();
        self.render_with_locale(template, ctx, &locale)
    }

    /// Detect the request locale from cookie / Accept-Language and render.
    pub fn render_with_headers(
        &self,
        template: &str,
        ctx: serde_json::Value,
        headers: &HeaderMap,
    ) -> Response {
        let locale = detect_locale(
            headers,
            self.i18n.available_locales(),
            self.i18n.default_locale(),
        );
        self.render_with_locale(template, ctx, &locale)
    }

    /// Render a template with a specific locale.
    pub fn render_with_locale(
        &self,
        template: &str,
        ctx: serde_json::Value,
        locale: &str,
    ) -> Response {
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

                // ── i18n context injection ──────────────────────────────
                map.entry("locale")
                    .or_insert_with(|| serde_json::Value::String(locale.to_string()));
                map.entry("t")
                    .or_insert_with(|| self.i18n.get_translations_json(locale));
                map.entry("available_locales").or_insert_with(|| {
                    serde_json::to_value(self.i18n.available_locales())
                        .unwrap_or(serde_json::Value::Array(vec![]))
                });
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

        // ── t filter: {{ "nav.dashboard" | t }} ──────────────────────────
        let i18n_t = self.i18n.clone();
        let locale_t = locale.to_string();
        tmpl_env.add_filter("t", move |key: &str| -> String { i18n_t.t(&locale_t, key) });

        // ── tvars filter: {{ "messages.saved" | tvars({"name": ext.name}) }} ─
        let i18n_tv = self.i18n.clone();
        let locale_tv = locale.to_string();
        tmpl_env.add_filter(
            "tvars",
            move |key: &str, vars: minijinja::Value| -> Result<String, minijinja::Error> {
                let vars_map: std::collections::HashMap<String, String> = if let Ok(obj) =
                    serde_json::from_str::<serde_json::Value>(
                        &serde_json::to_string(&vars).unwrap_or_default(),
                    ) {
                    if let serde_json::Value::Object(m) = obj {
                        m.into_iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                            .collect()
                    } else {
                        Default::default()
                    }
                } else {
                    Default::default()
                };
                Ok(i18n_tv.t_with_vars(&locale_tv, key, &vars_map))
            },
        );

        let base_path_for_fn = self.base_path().to_string();
        tmpl_env.add_function(
            "url_for",
            move |suffix: &str| -> Result<String, minijinja::Error> {
                let trimmed = suffix.trim();
                if trimmed.is_empty() || trimmed == "/" {
                    return Ok(format!("{}/", base_path_for_fn));
                }
                if trimmed.starts_with('/') {
                    if base_path_for_fn == "/" {
                        Ok(trimmed.to_string())
                    } else {
                        Ok(format!("{}{}", base_path_for_fn, trimmed))
                    }
                } else if base_path_for_fn == "/" {
                    Ok(format!("/{}", trimmed))
                } else {
                    Ok(format!("{}/{}", base_path_for_fn, trimmed))
                }
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

    pub async fn user_permissions(&self, user: &crate::models::user::Model) -> HashSet<String> {
        if user.is_superuser {
            let mut s = HashSet::new();
            s.insert("*".to_string());
            return s;
        }

        const TTL_SECS: u64 = 300;
        if let Ok(cache) = self.perm_cache.lock() {
            if let Some((ts, perms)) = cache.get(&user.id) {
                if ts.elapsed().as_secs() < TTL_SECS {
                    return perms.clone();
                }
            }
        }

        let perms = self.load_permissions_from_db(user.id).await;

        if let Ok(mut cache) = self.perm_cache.lock() {
            cache.insert(user.id, (Instant::now(), perms.clone()));
        }

        perms
    }

    async fn load_permissions_from_db(&self, user_id: i64) -> HashSet<String> {
        let role_ids = match user_role::Entity::find()
            .filter(user_role::Column::UserId.eq(user_id))
            .all(&self.db)
            .await
        {
            Ok(rows) => rows.into_iter().map(|r| r.role_id).collect::<Vec<_>>(),
            Err(_) => return HashSet::new(),
        };

        if role_ids.is_empty() {
            return HashSet::new();
        }

        match role_permission::Entity::find()
            .filter(role_permission::Column::RoleId.is_in(role_ids))
            .all(&self.db)
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .map(|p| format!("{}:{}", p.resource, p.action))
                .collect(),
            Err(_) => HashSet::new(),
        }
    }

    pub async fn has_permission(
        &self,
        user: &crate::models::user::Model,
        resource: &str,
        action: &str,
    ) -> bool {
        if user.is_superuser {
            return true;
        }
        let perms = self.user_permissions(user).await;
        perms.contains(&format!("{}:{}", resource, action))
    }

    pub async fn build_current_user_ctx(
        &self,
        user: &crate::models::user::Model,
    ) -> serde_json::Value {
        let perms = self.user_permissions(user).await;
        let perm_list: Vec<String> = if user.is_superuser {
            vec!["*".to_string()]
        } else {
            perms.iter().cloned().collect()
        };
        serde_json::json!({
            "id": user.id,
            "username": user.username,
            "email": user.email,
            "is_superuser": user.is_superuser,
            "is_staff": user.is_staff,
            "is_active": user.is_active,
            "permissions": perm_list,
            "can_manage_users": user.is_superuser || perms.contains("users:manage"),
            "can_write_system": user.is_superuser || perms.contains("system:write"),
        })
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    pub fn i18n(&self) -> &Arc<I18n> {
        &self.i18n
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConsoleConfig;
    use crate::models::migration::Migrator;
    use crate::models::rbac::{self, user_role};
    use chrono::Utc;
    use sea_orm::{ActiveValue::Set, Database};
    use sea_orm_migration::MigratorTrait;

    fn make_user(id: i64, is_superuser: bool) -> crate::models::user::Model {
        let now = Utc::now();
        crate::models::user::Model {
            id,
            email: format!("user{}@test.com", id),
            username: format!("user{}", id),
            password_hash: "x".into(),
            reset_token: None,
            reset_token_expires: None,
            last_login_at: None,
            last_login_ip: None,
            created_at: now,
            updated_at: now,
            is_active: true,
            is_staff: false,
            is_superuser,
            mfa_enabled: false,
            mfa_secret: None,
            auth_source: "local".into(),
        }
    }

    async fn setup_state() -> Arc<ConsoleState> {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        Migrator::up(&db, None).await.expect("run migrations");
        ConsoleState::initialize(
            Arc::new(crate::callrecord::DefaultCallRecordFormatter::default()),
            db,
            ConsoleConfig::default(),
        )
        .await
        .expect("initialize console state")
    }

    #[tokio::test]
    async fn superuser_has_wildcard_permission() {
        let state = setup_state().await;
        let user = make_user(1, true);
        let perms = state.user_permissions(&user).await;
        assert!(perms.contains("*"));
        assert!(state.has_permission(&user, "extensions", "delete").await);
        assert!(state.has_permission(&user, "ami", "access").await);
    }

    #[tokio::test]
    async fn user_without_roles_has_no_permissions() {
        let state = setup_state().await;
        let user = make_user(99, false);
        let perms = state.user_permissions(&user).await;
        assert!(perms.is_empty());
        assert!(!state.has_permission(&user, "extensions", "read").await);
    }

    #[tokio::test]
    async fn user_with_viewer_role_has_read_permissions() {
        let state = setup_state().await;
        let db = state.db();

        let roles = rbac::Entity::find().all(db).await.expect("query roles");
        let viewer = roles.iter().find(|r| r.name == "viewer").expect("viewer");

        let user = make_user(200, false);
        user_role::Entity::insert(user_role::ActiveModel {
            user_id: Set(user.id),
            role_id: Set(viewer.id),
            created_at: Set(Utc::now()),
            ..Default::default()
        })
        .exec(db)
        .await
        .expect("assign viewer role");

        let perms = state.user_permissions(&user).await;
        assert!(perms.contains("extensions:read"));
        assert!(perms.contains("cdr:read"));
        assert!(!perms.contains("extensions:write"));
        assert!(!perms.contains("extensions:delete"));
    }

    #[tokio::test]
    async fn build_current_user_ctx_contains_expected_fields() {
        let state = setup_state().await;
        let user = make_user(1, true);
        let ctx = state.build_current_user_ctx(&user).await;
        assert_eq!(ctx["id"], 1);
        assert_eq!(ctx["is_superuser"], true);
        assert_eq!(ctx["can_manage_users"], true);
        assert_eq!(ctx["can_write_system"], true);
        let perms = ctx["permissions"].as_array().expect("permissions array");
        assert!(perms.iter().any(|p| p == "*"));
    }

    #[tokio::test]
    async fn permission_cache_is_populated() {
        let state = setup_state().await;
        let user = make_user(300, false);
        state.user_permissions(&user).await;
        let cache = state.perm_cache.lock().expect("lock cache");
        assert!(cache.contains_key(&300));
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
