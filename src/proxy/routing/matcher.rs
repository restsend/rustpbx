use anyhow::{Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use rsipstack::{
    dialog::{authenticate::Credential, invitation::InviteOption},
    transport::SipAddr,
};
use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::Arc,
};
use tracing::info;

use crate::{
    call::{DialDirection, RoutingState, policy::PolicyCheckStatus},
    config::{DialplanHints, RouteResult},
    proxy::routing::{ActionType, RouteQueueConfig, RouteRule, SourceTrunk, TrunkConfig},
};

#[derive(Debug, Default, Clone)]
pub struct RouteTrace {
    pub matched_rule: Option<String>,
    pub selected_trunk: Option<String>,
    pub used_default_route: bool,
    pub rewrite_operations: Vec<String>,
    pub abort: Option<RouteAbortTrace>,
}

#[derive(Debug, Clone)]
pub struct RouteAbortTrace {
    pub code: u16,
    pub reason: Option<String>,
}

#[async_trait]
pub trait RouteResourceLookup: Send + Sync {
    async fn load_queue(&self, path: &str) -> Result<Option<RouteQueueConfig>>;
}

/// Main routing function
///
/// Routes INVITE requests based on configured routing rules and trunk configurations:
/// 1. Match routing rules by priority
/// 2. Apply rewrite rules
/// 3. Select target trunk
/// 4. Set destination, headers and credentials
#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    Execute,
    Inspect,
}

pub async fn match_invite(
    trunks: Option<&HashMap<String, TrunkConfig>>,
    routes: Option<&Vec<RouteRule>>,
    resource_lookup: Option<&dyn RouteResourceLookup>,
    option: InviteOption,
    origin: &rsip::Request,
    source_trunk: Option<&SourceTrunk>,
    routing_state: Arc<RoutingState>,
    direction: &DialDirection,
) -> Result<RouteResult> {
    match_invite_impl(
        trunks,
        routes,
        resource_lookup,
        option,
        origin,
        source_trunk,
        routing_state,
        direction,
        MatchMode::Execute,
        None,
    )
    .await
}

pub async fn match_invite_with_trace(
    trunks: Option<&HashMap<String, TrunkConfig>>,
    routes: Option<&Vec<RouteRule>>,
    resource_lookup: Option<&dyn RouteResourceLookup>,
    option: InviteOption,
    origin: &rsip::Request,
    source_trunk: Option<&SourceTrunk>,
    routing_state: Arc<RoutingState>,
    direction: &DialDirection,
    trace: &mut RouteTrace,
) -> Result<RouteResult> {
    match_invite_impl(
        trunks,
        routes,
        resource_lookup,
        option,
        origin,
        source_trunk,
        routing_state,
        direction,
        MatchMode::Execute,
        Some(trace),
    )
    .await
}

pub async fn inspect_invite(
    trunks: Option<&HashMap<String, TrunkConfig>>,
    routes: Option<&Vec<RouteRule>>,
    resource_lookup: Option<&dyn RouteResourceLookup>,
    option: InviteOption,
    origin: &rsip::Request,
    source_trunk: Option<&SourceTrunk>,
    routing_state: Arc<RoutingState>,
    direction: &DialDirection,
) -> Result<RouteResult> {
    match_invite_impl(
        trunks,
        routes,
        resource_lookup,
        option,
        origin,
        source_trunk,
        routing_state,
        direction,
        MatchMode::Inspect,
        None,
    )
    .await
}

async fn match_invite_impl(
    trunks: Option<&HashMap<String, TrunkConfig>>,
    routes: Option<&Vec<RouteRule>>,
    resource_lookup: Option<&dyn RouteResourceLookup>,
    option: InviteOption,
    origin: &rsip::Request,
    source_trunk: Option<&SourceTrunk>,
    routing_state: Arc<RoutingState>,
    direction: &DialDirection,
    mode: MatchMode,
    mut trace: Option<&mut RouteTrace>,
) -> Result<RouteResult> {
    let mut option = option;
    let routes = match routes {
        Some(routes) => routes,
        None => return Ok(RouteResult::NotHandled(option, None)),
    };

    // Extract URI information early to avoid borrowing conflicts
    let caller_user = option.caller.user().unwrap_or_default().to_string();
    let caller_host = option.caller.host().clone();
    let callee_user = option.callee.user().unwrap_or_default().to_string();
    let callee_host = option.callee.host().clone();
    let request_user = origin.uri.user().unwrap_or_default().to_string();
    let request_host = origin.uri.host().clone();

    info!(
        "Matching {:?} caller={}@{}, callee={}@{}, request={}@{}",
        direction, caller_user, caller_host, callee_user, callee_host, request_user, request_host
    );

    // Traverse routing rules by priority
    for rule in routes {
        if let Some(true) = rule.disabled {
            continue;
        }

        if !rule.direction.matches(direction) {
            continue;
        }

        if !rule.source_trunks.is_empty() {
            match source_trunk {
                Some(trunk)
                    if rule
                        .source_trunks
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&trunk.name)) => {}
                Some(_) => continue,
                None => continue,
            }
        }

        if !rule.source_trunk_ids.is_empty() {
            match source_trunk.and_then(|t| t.id) {
                Some(id) if rule.source_trunk_ids.iter().any(|rule_id| *rule_id == id) => {}
                _ => continue,
            }
        }

        // Check matching conditions
        let ctx = MatchContext {
            origin,
            caller_user: &caller_user,
            caller_host: &caller_host,
            callee_user: &callee_user,
            callee_host: &callee_host,
            request_user: &request_user,
            request_host: &request_host,
        };
        let rule_matched = matches_rule(rule, &ctx)?;

        if !rule_matched {
            continue;
        }

        // Resolve source trunk country
        let origin_country = if let Some(source) = source_trunk {
            trunks
                .and_then(|t| t.get(&source.name))
                .and_then(|c| c.country.as_deref())
        } else {
            None
        };

        let captures = collect_match_captures(rule, &ctx)?;

        if let Some(trace) = &mut trace {
            trace.matched_rule = Some(rule.name.clone());
        }

        // Apply rewrite rules
        let rewrites = if let Some(rewrite) = &rule.rewrite {
            if let Some(trace) = &mut trace {
                trace
                    .rewrite_operations
                    .extend(describe_rewrite_ops(rewrite));
            }
            apply_rewrite_rules(&mut option, rewrite, origin, &captures)?
        } else {
            HashMap::new()
        };

        info!(
            "Matched rule: {:?} action:{:?} rewrites:{:?}",
            rule.name, rule.action, rewrites
        );

        // Check Route Policy (using rewritten numbers)
        if let Some(policy) = &rule.policy {
            if let Some(guard) = &routing_state.policy_guard {
                let current_caller = option.caller.user().unwrap_or_default();
                let current_callee = option.callee.user().unwrap_or_default();

                if let PolicyCheckStatus::Rejected(rejection) = guard
                    .check_policy(policy, &current_caller, &current_callee, origin_country)
                    .await?
                {
                    let reason = rejection.to_string();
                    info!(
                        "Call rejected by route policy: {} reason: {}",
                        rule.name, reason
                    );
                    if let Some(trace) = &mut trace {
                        trace.abort = Some(RouteAbortTrace {
                            code: rsip::StatusCode::Forbidden.into(),
                            reason: Some(reason.clone()),
                        });
                    }
                    return Ok(RouteResult::Abort(
                        rsip::StatusCode::Forbidden,
                        Some(reason),
                    ));
                }
            }
        }

        let hints = if !rule.codecs.is_empty() {
            let mut hints = DialplanHints::default();
            hints.allow_codecs = Some(rule.codecs.clone());
            Some(hints)
        } else {
            None
        };

        // Handle based on action type
        match rule.action.get_action_type() {
            ActionType::Reject => {
                if let Some(reject_config) = &rule.action.reject {
                    let reason = reject_config.reason.clone();
                    info!(
                        "Rejecting call with code {} and reason: {:?}",
                        reject_config.code, reason
                    );
                    if let Some(trace) = &mut trace {
                        trace.abort = Some(RouteAbortTrace {
                            code: reject_config.code,
                            reason: reason.clone(),
                        });
                    }
                    return Ok(RouteResult::Abort(reject_config.code.into(), reason));
                } else {
                    if let Some(trace) = &mut trace {
                        trace.abort = Some(RouteAbortTrace {
                            code: rsip::StatusCode::Forbidden.into(),
                            reason: None,
                        });
                    }
                    return Ok(RouteResult::Abort(rsip::StatusCode::Forbidden, None));
                }
            }
            ActionType::Busy => {
                if let Some(trace) = &mut trace {
                    trace.abort = Some(RouteAbortTrace {
                        code: rsip::StatusCode::BusyHere.into(),
                        reason: None,
                    });
                }
                return Ok(RouteResult::Abort(rsip::StatusCode::BusyHere, None));
            }
            ActionType::Forward => {
                if let Some(dest_config) = &rule.action.dest {
                    if mode == MatchMode::Execute {
                        let selected_trunk = select_trunk(
                            dest_config,
                            &rule.action.select,
                            &rule.action.hash_key,
                            &option,
                            routing_state.clone(),
                        )?;

                        if let Some(trace) = &mut trace {
                            trace.selected_trunk = Some(selected_trunk.clone());
                        }

                        if let Some(trunk_config) = trunks
                            .as_ref()
                            .and_then(|trunks| trunks.get(&selected_trunk))
                        {
                            // Check Trunk Policy
                            if let Some(policy) = &trunk_config.policy {
                                if let Some(guard) = &routing_state.policy_guard {
                                    let current_caller = option.caller.user().unwrap_or_default();
                                    let current_callee = option.callee.user().unwrap_or_default();

                                    if let PolicyCheckStatus::Rejected(rejection) = guard
                                        .check_policy(
                                            policy,
                                            &current_caller,
                                            &current_callee,
                                            origin_country,
                                        )
                                        .await?
                                    {
                                        let reason = rejection.to_string();
                                        info!(
                                            "Call rejected by trunk policy: {} reason: {}",
                                            selected_trunk, reason
                                        );
                                        if let Some(trace) = &mut trace {
                                            trace.abort = Some(RouteAbortTrace {
                                                code: rsip::StatusCode::Forbidden.into(),
                                                reason: Some(reason.clone()),
                                            });
                                        }
                                        return Ok(RouteResult::Abort(
                                            rsip::StatusCode::Forbidden,
                                            Some(reason),
                                        ));
                                    }
                                }
                            }

                            apply_trunk_config(&mut option, trunk_config)?;
                            info!(
                                "Selected trunk: {} for destination: {}",
                                selected_trunk, trunk_config.dest
                            );
                        } else {
                            info!("Trunk '{}' not found in configuration", selected_trunk);
                        }
                    }
                }
                return Ok(RouteResult::Forward(option, hints));
            }
            ActionType::Queue => {
                let queue_ref = rule
                    .action
                    .queue
                    .as_ref()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("queue action requires a 'queue' reference"))?;

                let lookup = resource_lookup.ok_or_else(|| {
                    anyhow!(
                        "queue action cannot resolve '{}' without resource lookup",
                        queue_ref
                    )
                })?;

                // Try to resolve as ID if it looks like one (e.g. "123")
                // Or if it is prefixed with "queue:" which is stripped before calling this?
                // Actually, `rule.action.queue` comes from the route rule config.
                // If it's a file path, `load_queue` handles it.
                // If it's an ID, we need to handle it.
                
                // If the reference is just digits, treat it as a DB ID reference "db-<id>"
                let lookup_ref = if queue_ref.chars().all(|c| c.is_ascii_digit()) {
                    format!("db-{}", queue_ref)
                } else {
                    queue_ref.clone()
                };

                let queue_cfg = lookup
                    .load_queue(lookup_ref.as_str())
                    .await?
                    .ok_or_else(|| {
                        anyhow!("queue '{}' not found", queue_ref)
                    })?;
                let mut queue_plan = queue_cfg.to_queue_plan()?;
                if queue_plan.label.is_none() {
                    queue_plan.label = Some(queue_ref.clone());
                }
                let needs_trunk = queue_plan.dial_strategy.is_none();
                if needs_trunk {
                    let dest_config = rule.action.dest.as_ref().ok_or_else(|| {
                        anyhow!("queue action requires 'dest' or inline queue targets")
                    })?;

                    if mode == MatchMode::Execute {
                        let selected_trunk = select_trunk(
                            dest_config,
                            &rule.action.select,
                            &rule.action.hash_key,
                            &option,
                            routing_state.clone(),
                        )?;

                        if let Some(trace) = &mut trace {
                            trace.selected_trunk = Some(selected_trunk.clone());
                        }

                        if let Some(trunk_config) = trunks
                            .as_ref()
                            .and_then(|trunks| trunks.get(&selected_trunk))
                        {
                            // Check Trunk Policy
                            if let Some(policy) = &trunk_config.policy {
                                if let Some(guard) = &routing_state.policy_guard {
                                    let current_caller = option.caller.user().unwrap_or_default();
                                    let current_callee = option.callee.user().unwrap_or_default();

                                    if let PolicyCheckStatus::Rejected(rejection) = guard
                                        .check_policy(
                                            policy,
                                            &current_caller,
                                            &current_callee,
                                            origin_country,
                                        )
                                        .await?
                                    {
                                        let reason = rejection.to_string();
                                        info!(
                                            "Call rejected by trunk policy: {} reason: {}",
                                            selected_trunk, reason
                                        );
                                        if let Some(trace) = &mut trace {
                                            trace.abort = Some(RouteAbortTrace {
                                                code: rsip::StatusCode::Forbidden.into(),
                                                reason: Some(reason.clone()),
                                            });
                                        }
                                        return Ok(RouteResult::Abort(
                                            rsip::StatusCode::Forbidden,
                                            Some(reason),
                                        ));
                                    }
                                }
                            }
                            apply_trunk_config(&mut option, trunk_config)?;
                        }
                    }
                }

                return Ok(RouteResult::Queue {
                    option,
                    queue: queue_plan,
                    hints,
                });
            }
        }
    }

    return Ok(RouteResult::NotHandled(option, None));
}

/// Context for rule matching to reduce function arguments
struct MatchContext<'a> {
    origin: &'a rsip::Request,
    caller_user: &'a str,
    caller_host: &'a rsip::Host,
    callee_user: &'a str,
    callee_host: &'a rsip::Host,
    request_user: &'a str,
    request_host: &'a rsip::Host,
}

/// Check if routing rule matches
fn matches_rule(rule: &crate::proxy::routing::RouteRule, ctx: &MatchContext) -> Result<bool> {
    let conditions = &rule.match_conditions;

    // Check from.user
    if let Some(pattern) = &conditions.from_user {
        if !matches_pattern(pattern, ctx.caller_user)? {
            return Ok(false);
        }
    }

    // Check from.host
    if let Some(pattern) = &conditions.from_host {
        if !matches_pattern(pattern, &ctx.caller_host.to_string())? {
            return Ok(false);
        }
    }

    // Check to.user
    if let Some(pattern) = &conditions.to_user {
        if !matches_pattern(pattern, ctx.callee_user)? {
            return Ok(false);
        }
    }

    // Check to.host
    if let Some(pattern) = &conditions.to_host {
        if !matches_pattern(pattern, &ctx.callee_host.to_string())? {
            return Ok(false);
        }
    }

    // Check request_uri.user
    if let Some(pattern) = &conditions.request_uri_user {
        if !matches_pattern(pattern, ctx.request_user)? {
            return Ok(false);
        }
    }

    // Check request_uri.host
    if let Some(pattern) = &conditions.request_uri_host {
        if !matches_pattern(pattern, &ctx.request_host.to_string())? {
            return Ok(false);
        }
    }

    // Check compatibility fields
    if let Some(pattern) = &conditions.caller {
        let caller_full = format!("{}@{}", ctx.caller_user, ctx.caller_host);
        if !matches_pattern(pattern, &caller_full)? {
            return Ok(false);
        }
    }

    if let Some(pattern) = &conditions.callee {
        let callee_full = format!("{}@{}", ctx.callee_user, ctx.callee_host);
        if !matches_pattern(pattern, &callee_full)? {
            return Ok(false);
        }
    }

    // Check headers
    for (header_key, pattern) in &conditions.headers {
        if let Some(header_name) = header_key.strip_prefix("header.") {
            // Remove "header." prefix
            if let Some(header_value) = get_header_value(ctx.origin, header_name) {
                if !matches_pattern(pattern, &header_value)? {
                    return Ok(false);
                }
            } else {
                return Ok(false); // header not exist
            }
        }
    }

    Ok(true)
}

/// Collect capture groups from matched conditions to support rewrite templates
fn collect_match_captures(
    rule: &crate::proxy::routing::RouteRule,
    ctx: &MatchContext,
) -> Result<HashMap<String, Vec<String>>> {
    let mut captures = HashMap::new();
    let conditions = &rule.match_conditions;

    collect_field_capture(
        &mut captures,
        "from.user",
        conditions.from_user.as_deref(),
        ctx.caller_user,
    )?;

    let caller_host = ctx.caller_host.to_string();
    collect_field_capture(
        &mut captures,
        "from.host",
        conditions.from_host.as_deref(),
        &caller_host,
    )?;

    collect_field_capture(
        &mut captures,
        "to.user",
        conditions.to_user.as_deref(),
        ctx.callee_user,
    )?;

    let callee_host = ctx.callee_host.to_string();
    collect_field_capture(
        &mut captures,
        "to.host",
        conditions.to_host.as_deref(),
        &callee_host,
    )?;

    collect_field_capture(
        &mut captures,
        "request_uri.user",
        conditions.request_uri_user.as_deref(),
        ctx.request_user,
    )?;

    let request_host = ctx.request_host.to_string();
    collect_field_capture(
        &mut captures,
        "request_uri.host",
        conditions.request_uri_host.as_deref(),
        &request_host,
    )?;

    if let Some(pattern) = &conditions.caller {
        let caller_full = format!("{}@{}", ctx.caller_user, ctx.caller_host);
        collect_field_capture(
            &mut captures,
            "caller",
            Some(pattern.as_str()),
            &caller_full,
        )?;
    }

    if let Some(pattern) = &conditions.callee {
        let callee_full = format!("{}@{}", ctx.callee_user, ctx.callee_host);
        collect_field_capture(
            &mut captures,
            "callee",
            Some(pattern.as_str()),
            &callee_full,
        )?;
    }

    for (header_key, pattern) in &conditions.headers {
        if let Some(header_name) = header_key.strip_prefix("header.") {
            if let Some(value) = get_header_value(ctx.origin, header_name) {
                collect_field_capture(&mut captures, header_key, Some(pattern.as_str()), &value)?;
            }
        }
    }

    Ok(captures)
}

fn collect_field_capture(
    captures: &mut HashMap<String, Vec<String>>,
    key: &str,
    pattern: Option<&str>,
    value: &str,
) -> Result<()> {
    if let Some(pattern) = pattern {
        if let Some(groups) = extract_regex_captures(pattern, value)? {
            captures.insert(key.to_string(), groups);
        }
    }
    Ok(())
}

fn extract_regex_captures(pattern: &str, value: &str) -> Result<Option<Vec<String>>> {
    if pattern.is_empty() {
        return Ok(None);
    }

    // Compile pattern as regex to obtain capture groups
    let regex =
        Regex::new(pattern).map_err(|e| anyhow!("Invalid regex pattern '{}': {}", pattern, e))?;
    if let Some(captures) = regex.captures(value) {
        let mut groups = Vec::new();
        for index in 0..captures.len() {
            groups.push(
                captures
                    .get(index)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default(),
            );
        }
        return Ok(Some(groups));
    }

    Ok(None)
}

/// Match pattern (supports regex)
fn matches_pattern(pattern: &str, value: &str) -> Result<bool> {
    // If pattern doesn't contain regex special characters, use exact match
    if !pattern.contains('^')
        && !pattern.contains('$')
        && !pattern.contains('*')
        && !pattern.contains('+')
        && !pattern.contains('?')
        && !pattern.contains('[')
        && !pattern.contains('(')
        && !pattern.contains('\\')
    {
        return Ok(pattern == value);
    }

    // Use regex matching
    let regex =
        Regex::new(pattern).map_err(|e| anyhow!("Invalid regex pattern '{}': {}", pattern, e))?;
    Ok(regex.is_match(value))
}

/// Get header value
fn get_header_value(request: &rsip::Request, header_name: &str) -> Option<String> {
    for header in request.headers.iter() {
        match header {
            rsip::Header::Other(name, value)
                if name.to_lowercase() == header_name.to_lowercase() =>
            {
                return Some(value.clone());
            }
            rsip::Header::UserAgent(value) if header_name.to_lowercase() == "user-agent" => {
                return Some(value.to_string());
            }
            rsip::Header::Contact(contact) if header_name.to_lowercase() == "contact" => {
                return Some(contact.to_string());
            }
            // Add other standard header handling
            _ => continue,
        }
    }
    None
}

/// Apply rewrite rules
fn apply_rewrite_rules(
    option: &mut InviteOption,
    rewrite: &crate::proxy::routing::RewriteRules,
    origin: &rsip::Request,
    captures: &HashMap<String, Vec<String>>,
) -> Result<HashMap<String, String>> {
    let mut rewrites = HashMap::new();

    // Rewrite caller
    if let Some(pattern) = &rewrite.from_user {
        let new_user = apply_rewrite_pattern_with_match(
            pattern,
            option.caller.user().unwrap_or_default(),
            captures.get("from.user"),
        )?;
        option.caller = update_uri_user(&option.caller, &new_user)?;
        rewrites.insert("from.user".to_string(), new_user);
    }

    if let Some(pattern) = &rewrite.from_host {
        let current_host = option.caller.host().to_string();
        let new_host =
            apply_rewrite_pattern_with_match(pattern, &current_host, captures.get("from.host"))?;
        option.caller = update_uri_host(&option.caller, &new_host)?;
        rewrites.insert("from.host".to_string(), new_host);
    }

    // Rewrite callee
    if let Some(pattern) = &rewrite.to_user {
        let new_user = apply_rewrite_pattern_with_match(
            pattern,
            option.callee.user().unwrap_or_default(),
            captures.get("to.user"),
        )?;
        option.callee = update_uri_user(&option.callee, &new_user)?;
        rewrites.insert("to.user".to_string(), new_user);
    }

    if let Some(pattern) = &rewrite.to_host {
        let current_host = option.callee.host().to_string();
        let new_host =
            apply_rewrite_pattern_with_match(pattern, &current_host, captures.get("to.host"))?;
        option.callee = update_uri_host(&option.callee, &new_host)?;
        rewrites.insert("to.host".to_string(), new_host);
    }

    // Add or modify headers
    for (header_key, pattern) in &rewrite.headers {
        if let Some(header_name) = header_key.strip_prefix("header.") {
            let new_value = apply_rewrite_pattern(pattern, "", origin)?;

            let new_header = rsip::Header::Other(header_name.to_string(), new_value);

            if option.headers.is_none() {
                option.headers = Some(Vec::new());
            }
            option.headers.as_mut().unwrap().push(new_header);
        }
    }

    Ok(rewrites)
}

fn describe_rewrite_ops(rewrite: &crate::proxy::routing::RewriteRules) -> Vec<String> {
    let mut ops = Vec::new();

    let mut push = |label: &str, value: &Option<String>| {
        if value.as_ref().map(|v| !v.is_empty()).unwrap_or(false) {
            ops.push(label.to_string());
        }
    };

    push("from.user", &rewrite.from_user);
    push("from.host", &rewrite.from_host);
    push("to.user", &rewrite.to_user);
    push("to.host", &rewrite.to_host);
    push("to.port", &rewrite.to_port);
    push("request_uri.user", &rewrite.request_uri_user);
    push("request_uri.host", &rewrite.request_uri_host);
    push("request_uri.port", &rewrite.request_uri_port);

    for header in rewrite.headers.keys() {
        ops.push(header.to_string());
    }

    ops
}

/// Apply rewrite pattern (supports capture groups)
fn apply_rewrite_pattern_with_match(
    pattern: &str,
    original: &str,
    capture_groups: Option<&Vec<String>>,
) -> Result<String> {
    if !pattern.contains('{') {
        return Ok(pattern.to_string());
    }

    let mut result = String::new();
    let mut chars = pattern.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut index_buffer = String::new();
            let mut found_closing = false;

            while let Some(&next) = chars.peek() {
                chars.next();
                if next == '}' {
                    found_closing = true;
                    break;
                }
                index_buffer.push(next);
            }

            if !found_closing {
                return Err(anyhow!(
                    "Unclosed capture group placeholder in rewrite pattern '{}'",
                    pattern
                ));
            }

            if index_buffer.is_empty() {
                return Err(anyhow!(
                    "Empty capture group placeholder in rewrite pattern '{}'",
                    pattern
                ));
            }

            let index_value = index_buffer.parse::<usize>().map_err(|e| {
                anyhow!(
                    "Invalid capture group index '{}' in rewrite pattern '{}': {}",
                    index_buffer,
                    pattern,
                    e
                )
            })?;

            let replacement = capture_groups
                .and_then(|groups| groups.get(index_value).cloned())
                .or_else(|| {
                    if index_value == 0 {
                        Some(original.to_string())
                    } else {
                        extract_capture_group(original, index_value)
                    }
                })
                .unwrap_or_else(|| original.to_string());

            result.push_str(&replacement);
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

/// Extract capture group from common patterns
fn extract_capture_group(original: &str, group_num: usize) -> Option<String> {
    if group_num == 0 {
        return Some(original.to_string());
    }

    // Common regex patterns we support
    let patterns = [
        // (\d+) - any digits
        (r"^(\d+)$", vec![0]), // Group 1 is the entire string if all digits
        // prefix(\d+)suffix
        (r"^[^\d]*(\d+)[^\d]*$", vec![]), // Will be computed dynamically
    ];

    for (pattern_str, positions) in &patterns {
        if let Ok(regex) = Regex::new(pattern_str) {
            if let Some(captures) = regex.captures(original) {
                if group_num <= captures.len() && group_num > 0 {
                    if let Some(capture) = captures.get(group_num) {
                        return Some(capture.as_str().to_string());
                    }
                }
                // Fallback for simple position-based extraction
                if !positions.is_empty() && group_num == 1 {
                    let start_pos = positions[0];
                    if original.len() > start_pos {
                        // Extract digits from this position onward
                        let substr = &original[start_pos..];
                        let digits: String =
                            substr.chars().take_while(|c| c.is_ascii_digit()).collect();
                        if !digits.is_empty() {
                            return Some(digits);
                        }
                    }
                }
            }
        }
    }

    None
}

/// Apply rewrite pattern
fn apply_rewrite_pattern(pattern: &str, original: &str, _origin: &rsip::Request) -> Result<String> {
    // Support simple replacement patterns like "96123{1}" where {1} is capture group
    if pattern.contains('{') && pattern.contains('}') {
        // This is a pattern with capture groups, need to extract from original value
        // Simplified implementation: assume pattern is "prefix{1}suffix" format
        let start = pattern.find('{').unwrap();
        let end = pattern.find('}').unwrap();
        let prefix = &pattern[..start];
        let suffix = &pattern[end + 1..];
        let _group_num: usize = pattern[start + 1..end].parse().unwrap_or(1);

        // Should use previously matched capture groups here, simplified implementation returns original value
        Ok(format!("{}{}{}", prefix, original, suffix))
    } else {
        // Direct replacement
        Ok(pattern.to_string())
    }
}

/// Update URI user part
fn update_uri_user(uri: &rsip::Uri, new_user: &str) -> Result<rsip::Uri> {
    let mut new_uri = uri.clone();
    new_uri.auth = Some(rsip::Auth {
        user: new_user.to_string(),
        password: uri.auth.as_ref().and_then(|a| a.password.clone()),
    });
    Ok(new_uri)
}

/// Update URI host part
fn update_uri_host(uri: &rsip::Uri, new_host: &str) -> Result<rsip::Uri> {
    let mut new_uri = uri.clone();
    new_uri.host_with_port = new_host
        .try_into()
        .map_err(|e| anyhow!("Invalid host '{}': {:?}", new_host, e))?;
    Ok(new_uri)
}

/// Select trunk
fn select_trunk(
    dest_config: &crate::proxy::routing::DestConfig,
    select_method: &str,
    hash_key: &Option<String>,
    option: &InviteOption,
    routing_state: Arc<RoutingState>,
) -> Result<String> {
    let trunks = match dest_config {
        crate::proxy::routing::DestConfig::Single(trunk) => vec![trunk.clone()],
        crate::proxy::routing::DestConfig::Multiple(trunk_list) => trunk_list.clone(),
    };

    if trunks.is_empty() {
        return Err(anyhow!("No trunks configured"));
    }

    if trunks.len() == 1 {
        return Ok(trunks[0].clone());
    }

    match select_method {
        "random" => {
            use rand::Rng;
            let index = rand::rng().random_range(0..trunks.len());
            Ok(trunks[index].clone())
        }
        "hash" => {
            let hash_value = if let Some(key) = hash_key {
                match key.as_str() {
                    "from.user" => option.caller.user().unwrap_or_default().to_string(),
                    "to.user" => option.callee.user().unwrap_or_default().to_string(),
                    "call-id" => "default".to_string(), // Simplified implementation
                    _ => key.clone(),
                }
            } else {
                option.caller.to_string()
            };

            let mut hasher = DefaultHasher::new();
            hash_value.hash(&mut hasher);
            let index = (hasher.finish() as usize) % trunks.len();
            Ok(trunks[index].clone())
        }
        "rr" => {
            // Real round-robin implementation with state
            let destination_key = format!("{:?}", dest_config);
            let index = routing_state.next_round_robin_index(&destination_key, trunks.len());
            Ok(trunks[index].clone())
        }
        _ => {
            // Default to round-robin for unknown selection methods
            let destination_key = format!("{:?}", dest_config);
            let index = routing_state.next_round_robin_index(&destination_key, trunks.len());
            Ok(trunks[index].clone())
        }
    }
}

/// Apply trunk configuration
fn apply_trunk_config(option: &mut InviteOption, trunk: &TrunkConfig) -> Result<()> {
    // Set destination
    let dest_uri: rsip::Uri = trunk
        .dest
        .as_str()
        .try_into()
        .map_err(|e| anyhow!("Invalid trunk destination '{}': {:?}", trunk.dest, e))?;

    let transport = if let Some(transport_str) = &trunk.transport {
        match transport_str.to_lowercase().as_str() {
            "udp" => Some(rsip::transport::Transport::Udp),
            "tcp" => Some(rsip::transport::Transport::Tcp),
            "tls" => Some(rsip::transport::Transport::Tls),
            "ws" => Some(rsip::transport::Transport::Ws),
            "wss" => Some(rsip::transport::Transport::Wss),
            _ => None,
        }
    } else {
        None
    };

    option.destination = Some(SipAddr {
        r#type: transport,
        addr: dest_uri.host_with_port.clone(),
    });

    // Update callee host to match trunk destination
    option.callee.host_with_port = dest_uri.host_with_port.clone();

    // Set authentication info
    if let (Some(username), Some(password)) = (&trunk.username, &trunk.password) {
        option.credential = Some(Credential {
            username: username.clone(),
            password: password.clone(),
            realm: dest_uri.host().to_string().into(),
        });
    }

    // Add trunk related headers
    if option.headers.is_none() {
        option.headers = Some(Vec::new());
    }

    let headers = option.headers.as_mut().unwrap();

    // Add P-Asserted-Identity header
    if trunk.username.is_some() {
        let pai_header = rsip::Header::Other(
            "P-Asserted-Identity".to_string(),
            format!("<{}>", option.caller),
        );
        headers.push(pai_header);
    }

    Ok(())
}
