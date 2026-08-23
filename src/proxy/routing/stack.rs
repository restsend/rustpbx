//! Routing stack metadata for console visualization.
//!
//! Phase 1 is read-only: aggregates contributions from core config and addons
//! without changing runtime inspector order.

use crate::config::Config;
use serde::{Deserialize, Serialize};

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

/// Merge core + addon contributions and sort for display.
pub fn build_routing_stack(
    config: &Config,
    addon_contributions: Vec<RoutingContribution>,
) -> RoutingStackOverview {
    let mut contributions = core_routing_contributions(config);
    contributions.extend(addon_contributions);

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
    }
}

fn phase_order(phase: RoutingPhase) -> u8 {
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
        );
        let ids: Vec<_> = stack.contributions.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.iter().position(|&id| id == "core.emergency").unwrap()
            < ids.iter().position(|&id| id == "core.route_table").unwrap());
        assert!(ids.iter().position(|&id| id == "core.route_table").unwrap()
            < ids.iter().position(|&id| id == "voicemail.check").unwrap());
    }
}
