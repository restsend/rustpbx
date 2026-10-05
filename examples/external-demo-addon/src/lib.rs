use axum::routing::get;
use axum::Router;
use rustpbx::addons::{Addon, AuthAttempt};
use rustpbx::app::AppState;
use sea_orm_migration::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

pub struct DemoExternalAddon {
    auth_failures: Arc<AtomicUsize>,
    enabled: Arc<AtomicBool>,
}

impl Default for DemoExternalAddon {
    fn default() -> Self {
        Self {
            auth_failures: Arc::new(AtomicUsize::new(0)),
            enabled: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl DemoExternalAddon {
    pub fn auth_failure_count(&self) -> usize {
        self.auth_failures.load(Ordering::Relaxed)
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

pub struct NotesMigration;

impl MigrationName for NotesMigration {
    fn name(&self) -> &str {
        "demo_external_m0001_notes"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for NotesMigration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE IF NOT EXISTS demo_external_notes \
                 (id INTEGER PRIMARY KEY, note TEXT)",
            )
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Addon for DemoExternalAddon {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn id(&self) -> &'static str {
        "demo_external"
    }

    fn name(&self) -> &'static str {
        "Demo External"
    }

    fn description(&self) -> &'static str {
        "Out-of-tree SDK validation addon"
    }

    fn category(&self) -> rustpbx::addons::AddonCategory {
        rustpbx::addons::AddonCategory::Community
    }

    fn router(&self, _state: AppState) -> Option<Router> {
        let failures = Arc::clone(&self.auth_failures);
        Some(Router::new().route(
            "/api/demo_external/stats",
            get(move || {
                let failures = Arc::clone(&failures);
                async move {
                    axum::Json(serde_json::json!({
                        "auth_failures": failures.load(Ordering::Relaxed),
                    }))
                }
            }),
        ))
    }

    async fn initialize(&self, state: AppState) -> anyhow::Result<()> {
        let enabled = state
            .addon_config("demo_external")
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.enabled.store(enabled, Ordering::Relaxed);
        Ok(())
    }

    fn on_auth_attempt(&self, attempt: &AuthAttempt) {
        if attempt.outcome.is_failure() {
            self.auth_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(NotesMigration)]
    }

    fn template_dir(&self) -> Option<String> {
        Some("examples/external-demo-addon/templates".into())
    }

    fn static_dir(&self) -> Option<String> {
        Some("examples/external-demo-addon/static".into())
    }
}
