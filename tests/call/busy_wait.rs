//! Busy-wait (camp-on) configuration and plan-resolution tests.
//!
//! Covers the route-level `[busy_wait]` table at the config layer: TOML
//! parsing (nested table inside the flattened `RouteAction`), serde defaults,
//! serialization behaviour, and the conversion into the runtime `BusyWaitPlan`
//! carried on the dialplan.

use rustpbx::call::{BusyWaitPlan, DEFAULT_QUEUE_HOLD_AUDIO};
use rustpbx::config::DialplanHints;
use rustpbx::proxy::routing::{RouteAction, RouteBusyWaitConfig, RouteRule};

#[test]
fn busy_wait_table_parses_from_toml() {
    let input = r#"
name = "busy-wait-1001"
priority = 10

[match]
"to.user" = "1001"

[busy_wait]
max_wait_secs = 30
retry_interval_secs = 2
hold_audio = "sounds/moh.wav"
"#;
    let rule: RouteRule = toml::from_str(input).expect("parse route rule");
    let bw = rule
        .action
        .busy_wait
        .expect("nested [busy_wait] table must parse through the flattened action");
    assert!(bw.enabled, "enabled defaults to true");
    assert_eq!(bw.max_wait_secs, 30);
    assert_eq!(bw.retry_interval_secs, 2);
    assert_eq!(bw.hold_audio.as_deref(), Some("sounds/moh.wav"));
}

#[test]
fn busy_wait_absent_by_default() {
    let input = r#"
name = "plain-forward"

[match]
"to.user" = "1002"
"#;
    let rule: RouteRule = toml::from_str(input).expect("parse route rule");
    assert!(
        rule.action.busy_wait.is_none(),
        "no [busy_wait] table means no busy-wait policy"
    );
    assert!(RouteAction::default().busy_wait.is_none());
    assert!(DialplanHints::default().busy_wait.is_none());
}

#[test]
fn busy_wait_serde_defaults() {
    let input = r#"
name = "defaults"

[match]
"to.user" = "1003"

[busy_wait]
enabled = true
"#;
    let rule: RouteRule = toml::from_str(input).expect("parse route rule");
    let bw = rule.action.busy_wait.expect("busy_wait present");
    assert_eq!(bw.max_wait_secs, 60, "default max_wait_secs is 60");
    assert_eq!(
        bw.retry_interval_secs, 5,
        "default retry_interval_secs is 5"
    );
    assert_eq!(
        bw.hold_audio, None,
        "hold_audio falls back at to_plan() time"
    );
}

#[test]
fn busy_wait_to_plan_defaults_to_queue_hold_audio() {
    let plan = RouteBusyWaitConfig::default().to_plan();
    assert_eq!(plan.hold_audio, DEFAULT_QUEUE_HOLD_AUDIO);
    assert_eq!(plan.retry_interval, std::time::Duration::from_secs(5));
    assert_eq!(plan.max_wait, Some(std::time::Duration::from_secs(60)));
}

#[test]
fn busy_wait_to_plan_zero_max_wait_waits_indefinitely() {
    let plan = RouteBusyWaitConfig {
        max_wait_secs: 0,
        ..Default::default()
    }
    .to_plan();
    assert_eq!(
        plan.max_wait, None,
        "max_wait_secs = 0 must disable the wait budget"
    );
}

#[test]
fn busy_wait_to_plan_clamps_retry_interval() {
    let plan = RouteBusyWaitConfig {
        retry_interval_secs: 0,
        ..Default::default()
    }
    .to_plan();
    assert_eq!(
        plan.retry_interval,
        std::time::Duration::from_secs(1),
        "retry interval is floored at 1s to avoid a hot redial loop"
    );
}

#[test]
fn busy_wait_serialization_round_trip() {
    let input = r#"
name = "roundtrip"

[match]
"to.user" = "1004"

[busy_wait]
max_wait_secs = 15
retry_interval_secs = 3
"#;
    let rule: RouteRule = toml::from_str(input).expect("parse route rule");
    let serialized = toml::to_string(&rule).expect("serialize rule");
    assert!(
        serialized.contains("busy_wait") || serialized.contains("[busy_wait]"),
        "busy_wait must survive a serialize round trip: {serialized}"
    );

    let without = RouteRule {
        name: "no-busy-wait".to_string(),
        ..Default::default()
    };
    let serialized = toml::to_string(&without).expect("serialize rule");
    assert!(
        !serialized.contains("busy_wait"),
        "busy_wait must be skipped when absent: {serialized}"
    );
}

#[test]
fn busy_wait_plan_default_matches_documented_defaults() {
    let plan = BusyWaitPlan::default();
    assert_eq!(plan.max_wait, Some(std::time::Duration::from_secs(60)));
    assert_eq!(plan.retry_interval, std::time::Duration::from_secs(5));
    assert_eq!(plan.hold_audio, "sounds/phone-calling.wav");
}

#[test]
fn busy_wait_disabled_via_enabled_false() {
    let input = r#"
name = "disabled"

[match]
"to.user" = "1005"

[busy_wait]
enabled = false
"#;
    let rule: RouteRule = toml::from_str(input).expect("parse route rule");
    let bw = rule.action.busy_wait.expect("table still parsed");
    assert!(!bw.enabled, "enabled = false must round-trip");
}
