//! Built-in IVR definitions that ship with rustpbx — no `config/ivr/*.toml` required.
//!
//! Use the `builtin://<name>` file URI (or just the short name via
//! [`resolve_file_param`]) when starting the `ivr` app. Custom deployments can
//! still override any name by placing `config/ivr/<name>.toml` on disk; the
//! factory tries the filesystem first when the URI is a normal path.

use super::config::{EntryAction, IvrDefinition, MenuNode};
use std::collections::HashMap;

pub const URI_PREFIX: &str = "builtin://";

/// Session var: JSON params for the next `csat_survey` `start_app` (set by CC hook).
pub const CSAT_PARAMS_KEY: &str = "_csat_survey_params";

/// Return `Some(name)` when `file` is a built-in URI (`builtin://post_call_csat`).
pub fn parse_uri(file: &str) -> Option<&str> {
    file.strip_prefix(URI_PREFIX).filter(|s| !s.is_empty())
}

/// Preferred `file` param for an IVR name: built-in URI when registered.
pub fn resolve_file_param(name: &str) -> String {
    if get(name).is_some() {
        format!("{URI_PREFIX}{name}")
    } else {
        format!("config/ivr/{name}.toml")
    }
}

/// Look up a built-in IVR definition by short name.
pub fn get(name: &str) -> Option<IvrDefinition> {
    match name {
        "post_call_csat" => Some(post_call_csat()),
        "check_voicemail" => Some(check_voicemail()),
        _ => None,
    }
}

/// Post-call CSAT orchestrator: enters menu `csat` and immediately chains into
/// `csat_survey` (sub-second timeout). CSAT params come from [`CSAT_PARAMS_KEY`].
fn post_call_csat() -> IvrDefinition {
    let start_csat = EntryAction::StartApp {
        app: "csat_survey".to_string(),
        params: None,
        return_app: None,
        return_target: None,
        return_menu: None,
    };
    let mut menus = HashMap::new();
    menus.insert(
        "csat".to_string(),
        MenuNode {
            greeting: String::new(),
            timeout_ms: 1,
            max_retries: 0,
            timeout_action: Some(start_csat.clone()),
            unknown_key_action: Some(start_csat),
            ..Default::default()
        },
    );
    IvrDefinition {
        name: "post_call_csat".to_string(),
        description: Some("Built-in post-call CSAT entry (no TOML file)".into()),
        menus,
        ..Default::default()
    }
}

/// Check-voicemail (*97) entry: immediately chains into `check_voicemail`.
fn check_voicemail() -> IvrDefinition {
    let start = EntryAction::StartApp {
        app: "check_voicemail".to_string(),
        params: None,
        return_app: None,
        return_target: None,
        return_menu: None,
    };
    IvrDefinition {
        name: "check_voicemail".to_string(),
        description: Some("Built-in voicemail retrieval entry (no TOML file)".into()),
        root: Some(MenuNode {
            greeting: String::new(),
            timeout_ms: 1,
            max_retries: 0,
            timeout_action: Some(start.clone()),
            unknown_key_action: Some(start),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_builtin_uri() {
        assert_eq!(parse_uri("builtin://post_call_csat"), Some("post_call_csat"));
        assert_eq!(parse_uri("config/ivr/x.toml"), None);
    }

    #[test]
    fn resolve_prefers_builtin() {
        assert_eq!(
            resolve_file_param("post_call_csat"),
            "builtin://post_call_csat"
        );
        assert_eq!(
            resolve_file_param("my_custom_flow"),
            "config/ivr/my_custom_flow.toml"
        );
    }

    #[test]
    fn builtin_definitions_validate() {
        for name in ["post_call_csat", "check_voicemail"] {
            let def = get(name).expect(name);
            def.validate().expect("builtin IVR must validate");
        }
    }
}
