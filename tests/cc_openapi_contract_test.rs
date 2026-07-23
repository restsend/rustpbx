//! OpenAPI drift regression for the CC addon.
//!
//! Loads `cc-agent-cti.yaml` and asserts the drift fixes from 2026-07-04 stay
//! fixed. Each assertion corresponds to a concrete mismatch that existed before
//! (documented in `spec-drift-notes.md`).
//!
//! Also asserts the two spec copies (canonical + static) are byte-identical,
//! mirroring `scripts/check_openapi_sync.sh`.
//!
//! Run: cargo test --features addon-cc --test cc_openapi_contract_test -- --nocapture

#![cfg(feature = "addon-cc")]

use std::path::PathBuf;

fn spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/addons/cc/openapi/cc-agent-cti.yaml")
}

fn static_spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/addons/cc/static/cc-agent-cti.yaml")
}

fn load() -> String {
    std::fs::read_to_string(spec_path()).expect("read canonical spec")
}

/// Extract a whole schema block by name. Schemas sit at exactly 4 spaces of
/// indentation under `components.schemas:`. We scan for the next line that
/// starts with exactly 4 spaces followed by a non-space character.
fn schema_block<'a>(raw: &'a str, name: &str) -> &'a str {
    let needle = format!("    {}:", name);
    let idx = raw
        .find(&needle)
        .unwrap_or_else(|| panic!("schema {} not found", name));
    let after = &raw[idx + needle.len()..];
    let mut search = 0;
    let end: usize;
    loop {
        match after[search..].find("\n    ") {
            Some(p) => {
                let abs = search + p;
                // Byte right after "\n    " — if it's a non-space, this is a
                // sibling schema at the same 4-space indent.
                let boundary_byte = abs + "\n    ".len();
                let next_char = after
                    .as_bytes()
                    .get(boundary_byte)
                    .copied()
                    .unwrap_or(b'\n');
                if next_char != b' ' && next_char != b'\t' {
                    end = idx + needle.len() + abs;
                    break;
                }
                search = abs + 1;
            }
            None => {
                end = raw.len();
                break;
            }
        }
    }
    &raw[idx..end]
}

#[test]
fn test_spec_loads() {
    let raw = load();
    assert!(raw.contains("openapi:"), "missing openapi version");
    assert!(raw.contains("paths:"), "missing paths section");
}

#[test]
fn test_agents_create_documents_use_agent_id_as_endpoint() {
    // Drift #1
    let raw = load();
    let block = schema_block(&raw, "RegisterAgentRequest");
    assert!(
        block.contains("use_agent_id_as_endpoint:"),
        "use_agent_id_as_endpoint must be in RegisterAgentRequest"
    );
    assert!(
        block.contains("display_name"),
        "display_name must be documented"
    );
}

#[test]
fn test_agents_delete_does_not_document_404() {
    // Drift #3: code returns 204 always; 404 is unreachable.
    let raw = load();
    // Find the agents path-level delete block.
    let paths_idx = raw.find("paths:").unwrap();
    let agents_path = raw[paths_idx..]
        .find("/cc/agents/{agent_id}:")
        .map(|i| paths_idx + i)
        .unwrap();
    // The next top-level path starts at column 2 with a leading "/cc".
    let after_agents = &raw[agents_path..];
    let next_path = after_agents[1..]
        .find("\n  /cc")
        .map(|i| 1 + i)
        .unwrap_or(after_agents.len());
    let agents_block = &after_agents[..next_path];
    let del = agents_block.find("    delete:").unwrap();
    let del_block = &agents_block[del..];
    assert!(del_block.contains("204"), "DELETE must document 204");
    assert!(
        !del_block.contains("404"),
        "DELETE /cc/agents must NOT document 404 (code never returns it)"
    );
}

#[test]
fn test_skill_groups_delete_hard_delete_and_204() {
    // Drift #6
    let raw = load();
    let paths_idx = raw.find("paths:").unwrap();
    let sg_path = raw[paths_idx..]
        .find("/cc/skill-groups/{skill_group_id}:")
        .map(|i| paths_idx + i)
        .unwrap();
    let after = &raw[sg_path..];
    let next_path = after[1..]
        .find("\n  /cc")
        .map(|i| 1 + i)
        .unwrap_or(after.len());
    let block = &after[..next_path];
    let del = block.find("    delete:").unwrap();
    let del_block = &block[del..];
    assert!(del_block.contains("204"), "must document 204");
    assert!(
        del_block.contains("硬删除"),
        "summary must say hard delete (硬删除)"
    );
    assert!(
        !del_block.contains("404"),
        "DELETE skill-group must NOT document 404"
    );
}

#[test]
fn test_skill_group_response_documents_acd_policy() {
    // Drift #5
    let raw = load();
    assert!(
        raw.contains("acd_policy:"),
        "acd_policy must be documented (returned by list handler)"
    );
    // The list-not-returned fields must be marked nullable. Search a generous
    // window after each field occurrence (handles multi-line yaml).
    for field in &["overflow_groups", "sla_target_secs", "max_wait_secs"] {
        // Find the field under SkillGroupResponse (the 2nd occurrence for
        // overflow_groups is in CreateSkillGroupRequest — both must be nullable
        // in the Response, so search after "SkillGroupResponse").
        let sg_idx = raw.find("SkillGroupResponse:").expect("SkillGroupResponse");
        let search_zone = &raw[sg_idx..sg_idx + 2000];
        let fidx = search_zone
            .find(field)
            .unwrap_or_else(|| panic!("{} missing from SkillGroupResponse", field));
        // Char-boundary-safe window (descriptions contain multi-byte CJK).
        let mut end = (fidx + 150).min(search_zone.len());
        while !search_zone.is_char_boundary(end) {
            end -= 1;
        }
        let window = &search_zone[fidx..end];
        assert!(
            window.contains("nullable: true"),
            "{} must be nullable within 150 chars:\n{}",
            field,
            window
        );
    }
}

#[test]
fn test_transfers_config_uses_real_fields() {
    // Drift #9 (severe)
    let raw = load();
    let block = schema_block(&raw, "TransferConfigUpdate");
    for field in &[
        "blind_transfer_enabled",
        "consult_timeout_secs",
        "transfer_fail_action",
        "conference_on_transfer",
        "allow_agent_to_agent",
    ] {
        assert!(
            block.contains(field),
            "TransferConfigUpdate must document {}",
            field
        );
    }
    assert!(
        !block.contains("refer_enabled"),
        "refer_enabled was the old wrong field — must be gone"
    );
    assert!(
        !block.contains("attended_enabled"),
        "attended_enabled was the old wrong field — must be gone"
    );
    // GET must reference TransferConfig.
    let tcfg = schema_block(&raw, "TransferConfig");
    assert!(
        tcfg.contains("blind_transfer_enabled"),
        "TransferConfig schema must exist with real fields"
    );
}

#[test]
fn test_supervisor_sessions_has_required_fields() {
    // Drift #10 (severe)
    let raw = load();
    let block = schema_block(&raw, "MonitorStartRequest");
    for field in &[
        "supervisor_id",
        "target_call_id",
        "agent_leg",
        "monitor_type",
        "supervisor_call_id",
    ] {
        assert!(
            block.contains(field),
            "MonitorStartRequest must document {}",
            field
        );
    }
    let req_idx = block.find("required:").expect("required list");
    let req_block = &block[req_idx..req_idx + 150];
    assert!(
        req_block.contains("supervisor_id"),
        "supervisor_id in required"
    );
    assert!(req_block.contains("agent_leg"), "agent_leg in required");
}

#[test]
fn test_canonical_and_static_spec_are_identical() {
    let canonical = std::fs::read_to_string(spec_path()).expect("read canonical");
    let static_copy = std::fs::read_to_string(static_spec_path()).expect("read static");
    assert_eq!(
        canonical, static_copy,
        "canonical and static specs drifted — run: bash scripts/check_openapi_sync.sh --fix"
    );
}
