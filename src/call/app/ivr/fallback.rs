//! Step-IVR session-level fallback: match from/to/headers → built-in IVR.

use crate::config::{IvrFallbackConfig, IvrFallbackRule};
use crate::proxy::routing::MatchConditions;
use regex::Regex;
use std::collections::HashMap;
use tracing::warn;

/// Session variable / JumpIvr param marking that fallback was already used.
pub const IVR_FALLBACK_USED_KEY: &str = "ivr_fallback_used";

/// Resolve a fallback IVR name: first matching rule (by priority desc), else `default`.
pub fn resolve_fallback_target(
    config: &IvrFallbackConfig,
    caller: &str,
    callee: &str,
    sip_headers: Option<&HashMap<String, String>>,
) -> Option<String> {
    let mut rules: Vec<&IvrFallbackRule> = config.rules.iter().collect();
    rules.sort_by_key(|r| std::cmp::Reverse(r.priority));

    let (caller_user, caller_host) = split_addr(caller);
    let (callee_user, callee_host) = split_addr(callee);
    let empty = HashMap::new();
    let headers = sip_headers.unwrap_or(&empty);

    for rule in rules {
        if rule.target.is_empty() {
            continue;
        }
        match matches_session(
            &rule.match_conditions,
            &caller_user,
            &caller_host,
            &callee_user,
            &callee_host,
            headers,
        ) {
            Ok(true) => return Some(rule.target.clone()),
            Ok(false) => {}
            Err(e) => {
                warn!(
                    rule = rule.name.as_deref().unwrap_or(""),
                    error = %e,
                    "ivr_fallback rule match error, skipping"
                );
            }
        }
    }

    config.default.as_ref().filter(|s| !s.is_empty()).cloned()
}

fn split_addr(s: &str) -> (String, String) {
    // Strip optional sip:/sips: and <> wrappers used in some CallInfo paths.
    let s = s
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_start_matches("sip:")
        .trim_start_matches("sips:");
    if let Some((user, rest)) = s.split_once('@') {
        let host = rest.split([';', ';']).next().unwrap_or(rest);
        (user.to_string(), host.to_string())
    } else {
        (s.to_string(), String::new())
    }
}

fn matches_session(
    conditions: &MatchConditions,
    caller_user: &str,
    caller_host: &str,
    callee_user: &str,
    callee_host: &str,
    headers: &HashMap<String, String>,
) -> anyhow::Result<bool> {
    if let Some(pattern) = &conditions.from_user
        && !matches_pattern(pattern, caller_user)?
    {
        return Ok(false);
    }
    if let Some(pattern) = &conditions.from_host
        && !matches_pattern(pattern, caller_host)?
    {
        return Ok(false);
    }
    if let Some(pattern) = &conditions.to_user
        && !matches_pattern(pattern, callee_user)?
    {
        return Ok(false);
    }
    if let Some(pattern) = &conditions.to_host
        && !matches_pattern(pattern, callee_host)?
    {
        return Ok(false);
    }

    // Compatibility fields (same spirit as dialplan matcher).
    if let Some(pattern) = &conditions.caller {
        let full = if caller_host.is_empty() {
            caller_user.to_string()
        } else {
            format!("{caller_user}@{caller_host}")
        };
        if !matches_pattern(pattern, &full)? && !matches_pattern(pattern, caller_user)? {
            return Ok(false);
        }
    }
    if let Some(pattern) = &conditions.callee {
        let full = if callee_host.is_empty() {
            callee_user.to_string()
        } else {
            format!("{callee_user}@{callee_host}")
        };
        if !matches_pattern(pattern, &full)? && !matches_pattern(pattern, callee_user)? {
            return Ok(false);
        }
    }
    if let Some(pattern) = &conditions.from {
        let full = if caller_host.is_empty() {
            caller_user.to_string()
        } else {
            format!("{caller_user}@{caller_host}")
        };
        if !matches_pattern(pattern, &full)? && !matches_pattern(pattern, caller_user)? {
            return Ok(false);
        }
    }
    if let Some(pattern) = &conditions.to {
        let full = if callee_host.is_empty() {
            callee_user.to_string()
        } else {
            format!("{callee_user}@{callee_host}")
        };
        if !matches_pattern(pattern, &full)? && !matches_pattern(pattern, callee_user)? {
            return Ok(false);
        }
    }

    for (header_key, pattern) in &conditions.headers {
        if let Some(header_name) = header_key.strip_prefix("header.") {
            match header_lookup(headers, header_name) {
                Some(value) if matches_pattern(pattern, value)? => {}
                _ => return Ok(false),
            }
        }
    }

    Ok(true)
}

fn header_lookup<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    if let Some(v) = headers.get(name) {
        return Some(v.as_str());
    }
    let lower = name.to_lowercase();
    headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == lower)
        .map(|(_, v)| v.as_str())
}

/// Match pattern (supports regex) — mirrors routing matcher semantics.
fn matches_pattern(pattern: &str, value: &str) -> anyhow::Result<bool> {
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

    let regex = Regex::new(pattern)
        .map_err(|e| anyhow::anyhow!("Invalid regex pattern '{}': {}", pattern, e))?;
    Ok(regex.is_match(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IvrFallbackRule;

    fn cfg(rules: Vec<IvrFallbackRule>, default: Option<&str>) -> IvrFallbackConfig {
        IvrFallbackConfig {
            default: default.map(|s| s.to_string()),
            rules,
        }
    }

    #[test]
    fn resolve_prefers_higher_priority_rule() {
        let config = cfg(
            vec![
                IvrFallbackRule {
                    name: Some("low".into()),
                    priority: 10,
                    match_conditions: MatchConditions {
                        from_user: Some("1001".into()),
                        ..Default::default()
                    },
                    target: "low_ivr".into(),
                },
                IvrFallbackRule {
                    name: Some("high".into()),
                    priority: 100,
                    match_conditions: MatchConditions {
                        from_user: Some("1001".into()),
                        ..Default::default()
                    },
                    target: "high_ivr".into(),
                },
            ],
            Some("default_ivr"),
        );
        assert_eq!(
            resolve_fallback_target(&config, "1001", "4000", None).as_deref(),
            Some("high_ivr")
        );
    }

    #[test]
    fn resolve_falls_through_to_default() {
        let config = cfg(
            vec![IvrFallbackRule {
                name: Some("vip".into()),
                priority: 50,
                match_conditions: MatchConditions {
                    from_user: Some("^9".into()),
                    ..Default::default()
                },
                target: "vip_ivr".into(),
            }],
            Some("default_ivr"),
        );
        assert_eq!(
            resolve_fallback_target(&config, "1001", "4000", None).as_deref(),
            Some("default_ivr")
        );
        assert_eq!(
            resolve_fallback_target(&config, "9001", "4000", None).as_deref(),
            Some("vip_ivr")
        );
    }

    #[test]
    fn resolve_matches_header_case_insensitive() {
        let config = cfg(
            vec![IvrFallbackRule {
                name: Some("tenant".into()),
                priority: 1,
                match_conditions: MatchConditions {
                    headers: HashMap::from([("header.X-Tenant".into(), "acme".into())]),
                    ..Default::default()
                },
                target: "acme_ivr".into(),
            }],
            Some("default_ivr"),
        );
        let headers = HashMap::from([("x-tenant".into(), "acme".into())]);
        assert_eq!(
            resolve_fallback_target(&config, "1001", "4000", Some(&headers)).as_deref(),
            Some("acme_ivr")
        );
    }

    #[test]
    fn resolve_none_when_unconfigured() {
        let config = IvrFallbackConfig::default();
        assert!(resolve_fallback_target(&config, "1001", "4000", None).is_none());
    }
}
