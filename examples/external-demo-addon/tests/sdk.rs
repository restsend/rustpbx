use demo_external_addon::DemoExternalAddon;
use rustpbx::addons::registry::AddonRegistry;
use rustpbx::addons::{Addon, AuthAttempt, AuthAttemptOutcome};
use sea_orm::{ConnectionTrait, Statement};
use std::sync::Arc;

#[tokio::test]
async fn out_of_tree_addon_full_injection_path() {
    let addon = Arc::new(DemoExternalAddon::default());
    let registry = AddonRegistry::with_extra_addons(vec![addon.clone() as Arc<dyn Addon>]);

    let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
    registry.run_migrations(&db).await.unwrap();

    let backend = db.get_database_backend();
    let rows = db
        .query_all_raw(Statement::from_string(
            backend,
            "SELECT name FROM sqlite_master WHERE type='table' \
             AND name='demo_external_notes'",
        ))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "demo migration must have created its table");

    registry.dispatch_auth_attempt(&AuthAttempt {
        username: "1001".into(),
        realm: None,
        method: "REGISTER".into(),
        source: Some("9.9.9.9:5060".into()),
        outcome: AuthAttemptOutcome::BadCredentials,
    });
    registry.dispatch_auth_attempt(&AuthAttempt {
        username: "bob".into(),
        realm: None,
        method: "REGISTER".into(),
        source: Some("9.9.9.9:5060".into()),
        outcome: AuthAttemptOutcome::Success,
    });

    assert_eq!(addon.auth_failure_count(), 1);
}

#[test]
fn addon_config_section_reaches_the_addon_surface() {
    let raw = r#"
http_addr = "127.0.0.1:8088"

[proxy]
addr = "127.0.0.1"
udp_port = 15060
addons = ["demo_external"]

[addons.demo_external]
enabled = true
"#;
    let config: rustpbx::config::Config = toml::from_str(raw).unwrap();
    let section = config.addons.get("demo_external").expect("section present");
    assert_eq!(section.get("enabled"), Some(&toml::Value::Boolean(true)));
}

#[test]
fn addon_registry_template_and_static_dirs_resolve() {
    let addon = Arc::new(DemoExternalAddon::default());
    let registry = AddonRegistry::with_extra_addons(vec![addon as Arc<dyn Addon>]);

    let mut config = rustpbx::config::Config::default();
    config.proxy.addons = Some(vec!["demo_external".into()]);

    let template_dirs = registry.get_template_dirs(&config);
    assert_eq!(
        template_dirs.first().map(String::as_str),
        Some("examples/external-demo-addon/templates")
    );

    let mounts = registry.get_static_mounts(&config);
    assert_eq!(
        mounts,
        vec![(
            "demo_external".to_string(),
            "examples/external-demo-addon/static".to_string()
        )]
    );
}
