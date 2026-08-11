//! Cheap safety net for wholesale UI templates.
//!
//! Wholesale templates are loaded from disk at *runtime* by minijinja (see
//! `console::Console::render_with_locale`), so a deleted, renamed or
//! syntactically broken template only surfaces as an HTTP 500 when a user
//! actually visits the page — `cargo build` never notices. These tests catch
//! the two cheapest failure modes during `cargo test`:
//!
//!   1. A template file no longer exists on disk (deleted / renamed / typo in
//!      the handler's `render_with_headers("wholesale/…")` path).
//!   2. A template has a minijinja syntax error (unclosed `{% if %}`, bad
//!      expression, …).
//!
//! Keep [`TEMPLATES`] in sync with the files under
//! `src/addons/wholesale/templates/wholesale/`; the
//! `templates_list_is_in_sync_with_directory` test fails when it drifts.

use std::path::{Path, PathBuf};

/// Template directory relative to the crate root. Cargo runs tests with the
/// package root as the working directory, so this resolves in `cargo test`.
const TEMPLATE_DIR: &str = "src/addons/wholesale/templates/wholesale";

/// Every `.html` file rendered (or included) by the wholesale addon. If you add
/// or remove a template file, update this list — the sync test below fails when
/// it drifts from the directory contents.
const TEMPLATES: &[&str] = &[
    "_subnav.html",
    "cdrs.html",
    "cluster.html",
    "dashboard.html",
    "diagnostics.html",
    "export_tasks.html",
    "profile_detail.html",
    "profile_form.html",
    "profile_item_form.html",
    "profiles.html",
    "rate_deck_detail.html",
    "rate_deck_form.html",
    "rate_decks.html",
    "rate_import.html",
    "settings.html",
    "tenant_detail.html",
    "tenant_form.html",
    "tenant_trunk_form.html",
    "tenants.html",
    "wholesale_trunk_detail.html",
    "wholesale_trunk_form.html",
    "wholesale_trunks.html",
];

fn template_path(name: &str) -> PathBuf {
    Path::new(TEMPLATE_DIR).join(name)
}

#[test]
fn all_wholesale_templates_exist() {
    for name in TEMPLATES {
        let path = template_path(name);
        assert!(
            path.exists(),
            "wholesale template '{}' is referenced but missing at {}",
            name,
            path.display()
        );
    }
}

#[test]
fn templates_list_is_in_sync_with_directory() {
    let dir = Path::new(TEMPLATE_DIR);
    assert!(
        dir.is_dir(),
        "wholesale template directory missing: {}",
        dir.display()
    );

    let mut on_disk: Vec<String> = std::fs::read_dir(dir)
        .expect("read wholesale template dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".html"))
        .collect();
    on_disk.sort();

    let mut listed: Vec<String> = TEMPLATES.iter().map(|s| s.to_string()).collect();
    listed.sort();

    assert_eq!(
        listed, on_disk,
        "TEMPLATES list in template_check.rs drifted from the files on disk; \
         add/remove entries so they match"
    );
}

/// Syntax-check every template with minijinja. This mirrors the filters and
/// functions registered by `console::Console::render_with_locale` so that
/// compile-time resolution of custom filters (`t`, `tvars`, `format`, `json`)
/// and the `url_for` function behaves exactly like production. Only built when
/// the `console` feature (and thus minijinja) is enabled.
#[cfg(feature = "console")]
#[test]
fn all_wholesale_templates_parse() {
    let mut env = minijinja::Environment::new();

    // Stub implementations of the custom filters/functions. We only parse,
    // never render, so the bodies are irrelevant — the signatures must simply
    // match what the templates call.
    env.add_filter("t", |_key: &str| -> String { String::new() });
    env.add_filter(
        "tvars",
        |_key: &str, _vars: minijinja::Value| -> String { String::new() },
    );
    env.add_filter(
        "format",
        |_format_str: &str, value: minijinja::Value| -> String { value.to_string() },
    );
    env.add_filter(
        "json",
        |value: minijinja::Value| -> String { value.to_string() },
    );
    env.add_function("url_for", |suffix: &str| -> String { suffix.to_string() });

    for name in TEMPLATES {
        let path = template_path(name);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
        env.add_template_owned(name.to_string(), src).unwrap_or_else(|e| {
            panic!(
                "wholesale template '{}' has a minijinja syntax error: {}",
                name, e
            )
        });
    }
}

