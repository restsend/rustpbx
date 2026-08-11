use anyhow::Result;
use rustpbx::call::app::ivr::config::IvrFileConfig;

#[test]
fn test_ivr_basic_toml_parses() -> Result<()> {
    let toml_str = r#"
[ivr]
name = "basic-ivr"
description = "Test IVR"
lang = "zh-CN"
default_voice = "xiaoyan"

[ivr.root]
greeting = "welcome.gsm"
timeout_ms = 5000
max_retries = 3

[[ivr.root.entries]]
key = "1"
action = { type = "transfer", target = "sip:1001@127.0.0.1" }

[[ivr.root.entries]]
key = "2"
action = { type = "transfer", target = "sip:1002@127.0.0.1" }

[[ivr.root.entries]]
key = "*"
action = { type = "repeat" }
"#;

    let config: IvrFileConfig = toml::from_str(toml_str)?;
    assert_eq!(config.ivr.name, "basic-ivr");
    assert_eq!(config.ivr.description.as_deref(), Some("Test IVR"));
    assert_eq!(config.ivr.lang.as_deref(), Some("zh-CN"));
    assert_eq!(config.ivr.default_voice.as_deref(), Some("xiaoyan"));

    let root = config.ivr.root.expect("root menu should exist");
    assert_eq!(root.timeout_ms, 5000, "timeout_ms should be set");
    assert_eq!(root.max_retries, 3);
    assert_eq!(root.entries.len(), 3, "should have 3 entries");

    Ok(())
}

#[test]
fn test_ivr_dtmf_menu_transfer_action_parses() -> Result<()> {
    let toml_str = r#"
[ivr]
name = "dtmf-ivr"

[ivr.root]
greeting = "menu.gsm"
timeout_ms = 10000

[[ivr.root.entries]]
key = "1"
action = { type = "transfer", target = "extension:1001" }

[[ivr.root.entries]]
key = "2"
action = { type = "hangup", reason = "no-answer" }

[[ivr.root.entries]]
key = "9"
action = { type = "voicemail", target = "sip:1001@127.0.0.1" }
"#;

    let config: IvrFileConfig = toml::from_str(toml_str)?;
    let root = config.ivr.root.expect("root menu should exist");
    assert_eq!(root.entries.len(), 3, "should have 3 menu entries");

    let menu_keys: Vec<&str> = root.entries.iter().map(|m| m.key.as_str()).collect();
    assert!(menu_keys.contains(&"1"));
    assert!(menu_keys.contains(&"2"));
    assert!(menu_keys.contains(&"9"));

    Ok(())
}

#[test]
fn test_ivr_missing_root_still_parses() -> Result<()> {
    let toml_str = r#"
[ivr]
name = "empty-ivr"
"#;

    let config: IvrFileConfig = toml::from_str(toml_str)?;
    assert_eq!(config.ivr.name, "empty-ivr");
    assert!(config.ivr.root.is_none(), "root should be None when absent");
    Ok(())
}
