//! Routing stack metadata for console visualization and runtime ordering.
//!
//! Phase 2 applies `routing_stack.toml` overrides to inspector priority,
//! enabled state, and eval_mode, and sorts the post-resolve inspector chain.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// When an inspector runs relative to route-table resolution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvalMode {
    /// Evaluate after route resolution; may override non-empty dialplans.
    #[default]
    PreRoute,
    /// Only evaluate when the dialplan has no dial targets yet.
    PostRoute,
}

/// Per-contribution override persisted in `config/routing_stack.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContributionOverride {
    pub id: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub eval_mode: Option<EvalMode>,
}

/// On-disk routing stack overrides (optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingStackFileConfig {
    #[serde(default)]
    pub contribution: Vec<ContributionOverride>,
}

/// A routing-stack conflict warning for the console.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingStackWarning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_name: Option<String>,
}

/// Evaluation phase in the inbound routing pipeline.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPhase {
    /// Inspectors that may intercept before the user route table (Emergency, CC monitor).
    PreRoute,
    /// Console / TOML user route rules (`RouteRule`).
    RouteTable,
    /// Fallback when dialplan has no targets (*97, etc.).
    PostRoute,
    /// Extension locator resolution (same-realm registered contacts).
    ExtensionResolve,
    /// Wholesale tenant outbound trunk selection (inbound trunk only).
    OutboundWholesale,
    /// Session-time behavior (not a dialplan rule).
    SessionBehavior,
}

/// How to interpret `priority` within a phase.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PriorityDirection {
    /// Larger priority values are evaluated first (PBX routes, addon feature codes).
    HigherFirst,
    /// Smaller tier values are evaluated first (Wholesale profile items).
    LowerFirst,
}

/// A single routing rule contribution for the console routing stack view.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingContribution {
    pub id: String,
    /// `core`, `voicemail`, `cc`, `wholesale`, …
    pub source: String,
    pub label: String,
    pub phase: RoutingPhase,
    pub priority: i32,
    pub priority_direction: PriorityDirection,
    pub enabled: bool,
    pub match_summary: String,
    pub target_summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_mode: Option<String>,
    pub editable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Phase legend shown at the top of the routing console.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingPhaseInfo {
    pub phase: RoutingPhase,
    pub label: String,
    pub description: String,
    pub priority_direction: PriorityDirection,
}

/// Full routing stack snapshot for API / UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingStackOverview {
    pub phases: Vec<RoutingPhaseInfo>,
    pub contributions: Vec<RoutingContribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<RoutingStackWarning>,
}

pub fn routing_stack_path(config: &Config) -> PathBuf {
    config.config_dir().join("routing_stack.toml")
}

pub fn load_routing_stack_file(config: &Config) -> RoutingStackFileConfig {
    let path = routing_stack_path(config);
    if !path.exists() {
        return RoutingStackFileConfig::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|err| {
            tracing::warn!(path = %path.display(), error = %err, "failed to parse routing_stack.toml");
            RoutingStackFileConfig::default()
        }),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "failed to read routing_stack.toml");
            RoutingStackFileConfig::default()
        }
    }
}

pub fn save_routing_stack_file(
    config: &Config,
    file: &RoutingStackFileConfig,
) -> std::io::Result<()> {
    let path = routing_stack_path(config);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, content)
}

pub fn apply_overrides_to_contribution(
    contribution: &mut RoutingContribution,
    file: &RoutingStackFileConfig,
) {
    let Some(ov) = file.contribution.iter().find(|c| c.id == contribution.id) else {
        return;
    };
    if let Some(enabled) = ov.enabled {
        contribution.enabled = enabled;
    }
    if let Some(priority) = ov.priority {
        contribution.priority = priority;
    }
    if let Some(eval_mode) = ov.eval_mode {
        contribution.eval_mode = Some(eval_mode_to_str(eval_mode).to_string());
    }
}

pub fn apply_overrides_to_inspector_meta(
    id: &str,
    enabled: &mut bool,
    priority: &mut i32,
    eval_mode: &mut EvalMode,
    file: &RoutingStackFileConfig,
) {
    let Some(ov) = file.contribution.iter().find(|c| c.id == id) else {
        return;
    };
    if let Some(value) = ov.enabled {
        *enabled = value;
    }
    if let Some(value) = ov.priority {
        *priority = value;
    }
    if let Some(value) = ov.eval_mode {
        *eval_mode = value;
    }
}

pub fn upsert_contribution_override(
    file: &mut RoutingStackFileConfig,
    id: &str,
    enabled: Option<bool>,
    priority: Option<i32>,
    eval_mode: Option<EvalMode>,
) {
    if let Some(existing) = file.contribution.iter_mut().find(|c| c.id == id) {
        if let Some(value) = enabled {
            existing.enabled = Some(value);
        }
        if let Some(value) = priority {
            existing.priority = Some(value);
        }
        if let Some(value) = eval_mode {
            existing.eval_mode = Some(value);
        }
        return;
    }
    file.contribution.push(ContributionOverride {
        id: id.to_string(),
        enabled,
        priority,
        eval_mode,
    });
}

pub fn eval_mode_to_str(mode: EvalMode) -> &'static str {
    match mode {
        EvalMode::PreRoute => "pre_route",
        EvalMode::PostRoute => "post_route",
    }
}

pub fn eval_mode_from_str(value: &str) -> Option<EvalMode> {
    match value {
        "pre_route" => Some(EvalMode::PreRoute),
        "post_route" => Some(EvalMode::PostRoute),
        _ => None,
    }
}

pub fn default_eval_mode_for_id(id: &str) -> EvalMode {
    match id {
        "voicemail.check" => EvalMode::PostRoute,
        _ => EvalMode::PreRoute,
    }
}

pub fn default_priority_for_id(id: &str) -> i32 {
    match id {
        "core.emergency" => 9999,
        "core.number_pool" => 8900,
        "voicemail.check" => 8500,
        "cc.monitor" => 8400,
        _ => 100,
    }
}

pub fn default_phase_for_id(id: &str) -> RoutingPhase {
    match id {
        "core.emergency" | "core.number_pool" | "cc.monitor" => RoutingPhase::PreRoute,
        "voicemail.check" => RoutingPhase::PostRoute,
        _ => RoutingPhase::PreRoute,
    }
}

pub fn detect_shortcode_conflicts(
    shortcode: &str,
    routes: &[(i64, String, bool, &serde_json::Value)],
) -> Vec<RoutingStackWarning> {
    if shortcode.is_empty() {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    for (id, name, disabled, matchers) in routes {
        if *disabled {
            continue;
        }
        if route_may_match_shortcode(shortcode, matchers) {
            warnings.push(RoutingStackWarning {
                code: "shortcode_route_overlap".to_string(),
                message: format!(
                    "User route \"{name}\" may match check-voicemail shortcode \"{shortcode}\" before the *97 inspector runs"
                ),
                route_id: Some(*id),
                route_name: Some(name.clone()),
            });
        }
    }
    warnings
}

fn route_may_match_shortcode(shortcode: &str, matchers: &serde_json::Value) -> bool {
    let serialized = matchers.to_string();
    if serialized.contains(shortcode) {
        return true;
    }
    if let Some(obj) = matchers.as_object() {
        for key in ["callee", "callee_user", "request_user", "to_user", "regex"] {
            if let Some(value) = obj.get(key).and_then(|v| v.as_str())
                && (value == shortcode || value.contains(shortcode))
            {
                return true;
            }
        }
    }
    false
}

/// Detect overlap between a feature shortcode and console route patterns.
pub fn detect_route_pattern_conflicts(
    shortcode: &str,
    routes: &[(i64, String, bool, Option<&str>, Option<&str>)],
) -> Vec<RoutingStackWarning> {
    if shortcode.is_empty() {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    for (id, name, active, destination_pattern, source_pattern) in routes {
        if !active {
            continue;
        }
        let overlaps = [destination_pattern.as_deref(), source_pattern.as_deref()]
            .into_iter()
            .flatten()
            .any(|pattern| pattern.contains(shortcode));
        if overlaps {
            warnings.push(RoutingStackWarning {
                code: "shortcode_route_overlap".to_string(),
                message: format!(
                    "User route \"{name}\" pattern may match check-voicemail shortcode \"{shortcode}\" before the *97 inspector runs"
                ),
                route_id: Some(*id),
                route_name: Some(name.clone()),
            });
        }
    }
    warnings
}

pub fn default_phase_legend() -> Vec<RoutingPhaseInfo> {
    vec![
        RoutingPhaseInfo {
            phase: RoutingPhase::PreRoute,
            label: "Pre-route intercept".to_string(),
            description: "Emergency numbers and addon feature codes evaluated before user routes."
                .to_string(),
            priority_direction: PriorityDirection::HigherFirst,
        },
        RoutingPhaseInfo {
            phase: RoutingPhase::RouteTable,
            label: "User route table".to_string(),
            description: "Console / TOML RouteRule matchers (trunk, queue, IVR, app, forward)."
                .to_string(),
            priority_direction: PriorityDirection::HigherFirst,
        },
        RoutingPhaseInfo {
            phase: RoutingPhase::ExtensionResolve,
            label: "Extension resolution".to_string(),
            description: "Same-realm callee resolved via Locator to registered SIP contacts."
                .to_string(),
            priority_direction: PriorityDirection::HigherFirst,
        },
        RoutingPhaseInfo {
            phase: RoutingPhase::PostRoute,
            label: "Post-route fallback".to_string(),
            description: "Addon inspectors when no route target was produced (*97, etc.)."
                .to_string(),
            priority_direction: PriorityDirection::HigherFirst,
        },
        RoutingPhaseInfo {
            phase: RoutingPhase::OutboundWholesale,
            label: "Wholesale outbound".to_string(),
            description: "Carrier inbound trunk → tenant profile → outbound SIP trunk (LCR)."
                .to_string(),
            priority_direction: PriorityDirection::LowerFirst,
        },
        RoutingPhaseInfo {
            phase: RoutingPhase::SessionBehavior,
            label: "Session behavior".to_string(),
            description: "Runtime chaining after dial (e.g. no-answer → voicemail)."
                .to_string(),
            priority_direction: PriorityDirection::HigherFirst,
        },
    ]
}

/// Build core (non-addon) routing contributions from proxy config.
pub fn core_routing_contributions(config: &Config) -> Vec<RoutingContribution> {
    let mut out = Vec::new();

    if let Some(emg) = config.proxy.emergency.as_ref() {
        let numbers = emg.numbers.join(", ");
        out.push(RoutingContribution {
            id: "core.emergency".to_string(),
            source: "core".to_string(),
            label: "Emergency routing".to_string(),
            phase: RoutingPhase::PreRoute,
            priority: 9999,
            priority_direction: PriorityDirection::HigherFirst,
            enabled: emg.enabled,
            match_summary: format!("to.user in [{numbers}]"),
            target_summary: format!("trunk:{}", emg.emergency_trunk),
            eval_mode: Some("pre_route".to_string()),
            editable: false,
            config_url: None,
            notes: Some("Configured in proxy TOML [emergency].".to_string()),
        });
    }

    out.push(RoutingContribution {
        id: "core.number_pool".to_string(),
        source: "core".to_string(),
        label: "Number pool (DID assignment)".to_string(),
        phase: RoutingPhase::PreRoute,
        priority: 8900,
        priority_direction: PriorityDirection::HigherFirst,
        enabled: true,
        match_summary: "inbound trunk with number pool".to_string(),
        target_summary: "least-used DID rewrite".to_string(),
        eval_mode: Some("pre_route".to_string()),
        editable: false,
        config_url: None,
        notes: None,
    });

    out.push(RoutingContribution {
        id: "core.route_table".to_string(),
        source: "core".to_string(),
        label: "PBX user routes".to_string(),
        phase: RoutingPhase::RouteTable,
        priority: 100,
        priority_direction: PriorityDirection::HigherFirst,
        enabled: true,
        match_summary: "RouteRule matchers (regex, trunk, country, …)".to_string(),
        target_summary: "forward / queue / ivr / app / trunk".to_string(),
        eval_mode: None,
        editable: true,
        config_url: None,
        notes: Some(
            "Default priority 100; higher values match first. Table below lists all rules."
                .to_string(),
        ),
    });

    out.push(RoutingContribution {
        id: "core.extension_locator".to_string(),
        source: "core".to_string(),
        label: "Extension locator".to_string(),
        phase: RoutingPhase::ExtensionResolve,
        priority: 50,
        priority_direction: PriorityDirection::HigherFirst,
        enabled: true,
        match_summary: "same-realm callee, route preview NotHandled".to_string(),
        target_summary: "registered SIP contact(s)".to_string(),
        eval_mode: None,
        editable: false,
        config_url: None,
        notes: Some("parallel_fork controls multi-device ring.".to_string()),
    });

    if config.proxy.route_originated_calls {
        out.push(RoutingContribution {
            id: "core.route_originated".to_string(),
            source: "core".to_string(),
            label: "Originated-call re-route".to_string(),
            phase: RoutingPhase::RouteTable,
            priority: 40,
            priority_direction: PriorityDirection::HigherFirst,
            enabled: true,
            match_summary: "dynamic leg / transfer external target".to_string(),
            target_summary: "second pass through route table".to_string(),
            eval_mode: None,
            editable: false,
            config_url: None,
            notes: Some("proxy.route_originated_calls = true".to_string()),
        });
    }

    out
}

/// Merge core + addon contributions, apply overrides, and sort for display.
pub fn build_routing_stack(
    config: &Config,
    addon_contributions: Vec<RoutingContribution>,
    warnings: Vec<RoutingStackWarning>,
) -> RoutingStackOverview {
    let file = load_routing_stack_file(config);
    let mut contributions = core_routing_contributions(config);
    contributions.extend(addon_contributions);
    for contribution in &mut contributions {
        apply_overrides_to_contribution(contribution, &file);
    }

    contributions.sort_by(|a, b| {
        phase_order(a.phase)
            .cmp(&phase_order(b.phase))
            .then_with(|| match a.priority_direction {
                PriorityDirection::HigherFirst => b.priority.cmp(&a.priority),
                PriorityDirection::LowerFirst => a.priority.cmp(&b.priority),
            })
            .then_with(|| a.id.cmp(&b.id))
    });

    RoutingStackOverview {
        phases: default_phase_legend(),
        contributions,
        warnings,
    }
}

pub fn phase_order(phase: RoutingPhase) -> u8 {
    match phase {
        RoutingPhase::PreRoute => 0,
        RoutingPhase::RouteTable => 1,
        RoutingPhase::ExtensionResolve => 2,
        RoutingPhase::PostRoute => 3,
        RoutingPhase::OutboundWholesale => 4,
        RoutingPhase::SessionBehavior => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_route_pattern_conflicts_finds_overlap() {
        let warnings = detect_route_pattern_conflicts(
            "*97",
            &[(1, "vm-shortcut".to_string(), true, Some("*97"), None)],
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "shortcode_route_overlap");
    }

    #[test]
    fn routing_stack_override_disables_contribution() {
        let mut contribution = RoutingContribution {
            id: "voicemail.check".to_string(),
            source: "voicemail".to_string(),
            label: "Check voicemail".to_string(),
            phase: RoutingPhase::PostRoute,
            priority: 8500,
            priority_direction: PriorityDirection::HigherFirst,
            enabled: true,
            match_summary: "*97".to_string(),
            target_summary: "app".to_string(),
            eval_mode: Some("post_route".to_string()),
            editable: true,
            config_url: None,
            notes: None,
        };
        let file = RoutingStackFileConfig {
            contribution: vec![ContributionOverride {
                id: "voicemail.check".to_string(),
                enabled: Some(false),
                priority: None,
                eval_mode: None,
            }],
        };
        apply_overrides_to_contribution(&mut contribution, &file);
        assert!(!contribution.enabled);
    }

    #[test]
    fn build_routing_stack_sorts_by_phase_then_priority() {
        let mut config = Config::default();
        config.proxy.emergency = Some(crate::config::EmergencyConfig {
            enabled: true,
            numbers: vec!["911".into()],
            emergency_trunk: "emergency".into(),
        });
        let stack = build_routing_stack(
            &config,
            vec![RoutingContribution {
                id: "voicemail.check".to_string(),
                source: "voicemail".to_string(),
                label: "Check voicemail".to_string(),
                phase: RoutingPhase::PostRoute,
                priority: 8500,
                priority_direction: PriorityDirection::HigherFirst,
                enabled: true,
                match_summary: "to.user = *97".to_string(),
                target_summary: "app:check_voicemail".to_string(),
                eval_mode: Some("post_route".to_string()),
                editable: true,
                config_url: None,
                notes: None,
            }],
            vec![],
        );
        let ids: Vec<_> = stack.contributions.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.iter().position(|&id| id == "core.emergency").unwrap()
            < ids.iter().position(|&id| id == "core.route_table").unwrap());
        assert!(ids.iter().position(|&id| id == "core.route_table").unwrap()
            < ids.iter().position(|&id| id == "voicemail.check").unwrap());
    }
}
