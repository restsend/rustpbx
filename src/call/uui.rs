//! RFC 7433 SIP User-to-User (UUI) header handling for call-center session
//! correlation.
//!
//! Line format used by rustpbx (CC addon):
//!
//! ```text
//! User-to-User: <session_id>;encoding=hex;purpose=call-center;queue=<id>;qn=<urlencoded-name>;skill=<group>;leg=<role>
//! ```
//!
//! - `session_id` — the root session identifier (first INVITE Call-ID of the
//!   call, or a generated root id for originates). Constant across all child
//!   legs (queue dispatch, REFER transfers, consultations).
//! - `purpose=call-center` — the RFC 7433 registered purpose for call-center
//!   data. Headers without this purpose are ignored by the CC integration.
//! - `queue` / `qn` / `skill` — optional CC context (queue canonical key,
//!   human-readable queue name, skill-group id).
//! - `leg` — optional declared dial intent of this leg (`agent` for
//!   queue/skill-group dispatch, `consult` for attended-transfer consult,
//!   `transfer` for REFER transfer targets). A downstream cluster node that
//!   terminates the INVITE as its own session uses it to tell a child leg of
//!   an existing logical call apart from a new call. Only trusted from
//!   cluster peers; old peers ignore the unknown param.
//!
//! Plain p2p / wholesale calls never carry this header: it is only injected by
//! the CC enricher and the transfer paths.

use rsipstack::sip::Header;

pub const UUI_HEADER_NAME: &str = "User-to-User";
pub const UUI_PURPOSE: &str = "call-center";

/// Parsed CC UUI information.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CcUui {
    /// Root session id — correlates every leg of one logical call.
    pub session_id: String,
    /// Queue canonical key (machine identifier).
    pub queue_id: Option<String>,
    /// Human-readable queue name (URL-decoded).
    pub queue_name: Option<String>,
    /// Skill-group id.
    pub skill_group: Option<String>,
    /// Declared dial intent of this leg (see the module docs). Whitespace
    /// values are dropped; unknown roles are kept as-is — the consuming node
    /// decides what to do with them.
    pub leg: Option<String>,
}

/// Leg roles the `leg=` UUI parameter may carry. Each maps to a dial intent
/// the owner session declares on the outgoing INVITE.
pub const LEG_ROLES: [&str; 3] = ["agent", "consult", "transfer"];

/// True when `role` is a known `leg=` UUI parameter value.
pub fn is_known_leg_role(role: &str) -> bool {
    LEG_ROLES.contains(&role)
}

/// Parse a `User-to-User` header value into [`CcUui`].
///
/// Returns `None` when the header is not a CC UUI (missing
/// `purpose=call-center`) or the UUI data is empty.
pub fn parse_uui(value: &str) -> Option<CcUui> {
    let mut parts = value.split(';');
    let data = parts.next().unwrap_or_default().trim();
    if data.is_empty() {
        return None;
    }

    let mut is_call_center = false;
    let mut queue_id = None;
    let mut queue_name = None;
    let mut skill_group = None;
    let mut leg = None;
    for param in parts {
        let param = param.trim();
        let (k, v) = match param.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => (param.to_ascii_lowercase(), ""),
        };
        match k.as_str() {
            "purpose" => is_call_center = v.eq_ignore_ascii_case(UUI_PURPOSE),
            "queue" => queue_id = Some(v.to_string()),
            "qn" => {
                queue_name = Some(
                    urlencoding::decode(v)
                        .map(|s| s.into_owned())
                        .unwrap_or_else(|_| v.to_string()),
                )
            }
            "skill" => skill_group = Some(v.to_string()),
            "leg" if !v.is_empty() => leg = Some(v.to_string()),
            _ => {}
        }
    }

    if !is_call_center {
        return None;
    }
    Some(CcUui {
        session_id: data.to_string(),
        queue_id,
        queue_name,
        skill_group,
        leg,
    })
}

/// Extract the first CC UUI (`purpose=call-center`) from a header list.
pub fn extract_cc_uui(headers: &rsipstack::sip::Headers) -> Option<CcUui> {
    headers.iter().find_map(|h| {
        if !h.name().eq_ignore_ascii_case(UUI_HEADER_NAME) {
            return None;
        }
        parse_uui(&h.value())
    })
}

/// Device-declared dial intent for a SELF-dialed leg (restsend-call tags its
/// consult / Replaces transfer-target INVITEs) arrives on the SAME RFC 7433
/// `User-to-User` header as the cluster dispatch marker, with a fixed
/// NON-UUID data constant (`self-dial` — restsend cannot echo its wire
/// Call-ID at header-build time, and a UUID-shaped device id must NOT be
/// trusted as a correlation root):
///
/// ```text
/// User-to-User: self-dial;encoding=ascii;purpose=call-center;leg=consult
/// ```
///
/// (`X-*` headers are not allowed on the wire.) Trust is split by source in
/// the session constructor:
///
/// - from a **cluster peer** → `meta.child_leg_role` — a child leg of a
///   call dispatched by another node (lifecycle suppression + `leg_role`
///   stamping on RWI events);
/// - from anyone else (the agent's own device) → `meta.dial_leg_role` — a
///   stats marker ONLY: persisted nowhere today, and never shaping the
///   call lifecycle. Kept in session meta so agent-metric exclusions can
///   be reintroduced on top of the parsed value when needed.
pub const LEG_ROLE_CONSULT: &str = "consult";
pub const LEG_ROLE_TRANSFER: &str = "transfer";

/// Build a `User-to-User` header value for the CC purpose.
pub fn build_uui_value(
    session_id: &str,
    queue_id: Option<&str>,
    queue_name: Option<&str>,
    skill_group: Option<&str>,
    leg: Option<&str>,
) -> String {
    let mut value = format!("{};encoding=hex;purpose={}", session_id, UUI_PURPOSE);
    if let Some(q) = queue_id.filter(|s| !s.is_empty()) {
        value.push_str(";queue=");
        value.push_str(q);
    }
    if let Some(qn) = queue_name.filter(|s| !s.is_empty()) {
        value.push_str(";qn=");
        value.push_str(&urlencoding::encode(qn));
    }
    if let Some(s) = skill_group.filter(|s| !s.is_empty()) {
        value.push_str(";skill=");
        value.push_str(s);
    }
    if let Some(role) = leg.filter(|s| !s.is_empty()) {
        value.push_str(";leg=");
        value.push_str(role);
    }
    value
}

/// Build the `User-to-User` SIP header.
pub fn build_uui_header(
    session_id: &str,
    queue_id: Option<&str>,
    queue_name: Option<&str>,
    skill_group: Option<&str>,
    leg: Option<&str>,
) -> Header {
    Header::Other(
        UUI_HEADER_NAME.to_string(),
        build_uui_value(session_id, queue_id, queue_name, skill_group, leg),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip_full() {
        let value = build_uui_value(
            "abc123",
            Some("sales"),
            Some("Sales Hotline"),
            Some("sg-1"),
            None,
        );
        let uui = parse_uui(&value).expect("must parse");
        assert_eq!(uui.session_id, "abc123");
        assert_eq!(uui.queue_id.as_deref(), Some("sales"));
        assert_eq!(uui.queue_name.as_deref(), Some("Sales Hotline"));
        assert_eq!(uui.skill_group.as_deref(), Some("sg-1"));
        assert!(uui.leg.is_none());
    }

    #[test]
    fn parse_minimal() {
        let uui = parse_uui("deadbeef;encoding=hex;purpose=call-center").unwrap();
        assert_eq!(uui.session_id, "deadbeef");
        assert!(uui.queue_id.is_none());
        assert!(uui.queue_name.is_none());
        assert!(uui.leg.is_none());
    }

    #[test]
    fn leg_param_roundtrip() {
        for role in LEG_ROLES {
            assert!(is_known_leg_role(role));
            let value = build_uui_value("root-1", None, None, None, Some(role));
            let uui = parse_uui(&value).expect("must parse");
            assert_eq!(uui.leg.as_deref(), Some(role), "role={role}");
            assert_eq!(uui.session_id, "root-1");
        }
    }

    #[test]
    fn leg_param_empty_is_dropped() {
        let value = build_uui_value("root-1", None, None, None, Some(""));
        let uui = parse_uui(&value).unwrap();
        assert!(uui.leg.is_none());
    }

    /// `parse_uui` keeps unknown roles (a newer node may introduce one); the
    /// consuming node applies its own whitelist.
    #[test]
    fn leg_param_unknown_role_is_kept() {
        let uui = parse_uui("root-1;encoding=hex;purpose=call-center;leg=whatever").unwrap();
        assert_eq!(uui.leg.as_deref(), Some("whatever"));
        assert!(!is_known_leg_role("whatever"));
    }

    #[test]
    fn known_leg_roles_whitelist() {
        assert!(is_known_leg_role("agent"));
        assert!(is_known_leg_role("consult"));
        assert!(is_known_leg_role("transfer"));
        assert!(!is_known_leg_role(""));
        assert!(!is_known_leg_role("Agent"));
    }

    #[test]
    fn rejects_non_cc_purpose() {
        assert!(parse_uui("deadbeef;encoding=hex").is_none());
        assert!(parse_uui("deadbeef;encoding=hex;purpose=foo").is_none());
    }

    #[test]
    fn rejects_empty_data() {
        assert!(parse_uui(";encoding=hex;purpose=call-center").is_none());
        assert!(parse_uui("").is_none());
    }

    #[test]
    fn queue_name_url_decoded() {
        let value = build_uui_value("s1", None, Some("支持中心 中文"), None, None);
        let uui = parse_uui(&value).unwrap();
        assert_eq!(uui.queue_name.as_deref(), Some("支持中心 中文"));
    }

    #[test]
    fn extract_from_headers() {
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(Header::Other("X-Ignored".into(), "1".into()));
        headers.push(Header::Other(
            UUI_HEADER_NAME.into(),
            build_uui_value("sid-1", Some("q1"), None, None, None),
        ));
        let uui = extract_cc_uui(&headers).expect("must find");
        assert_eq!(uui.session_id, "sid-1");
        assert_eq!(uui.queue_id.as_deref(), Some("q1"));
    }

    #[test]
    fn extract_missing_returns_none() {
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(Header::Other("X-Other".into(), "v".into()));
        assert!(extract_cc_uui(&headers).is_none());
    }

    /// Device-declared dial intent (restsend-call consult / Replaces
    /// transfer INVITEs) rides the SAME UUI `leg=` parameter as the cluster
    /// dispatch marker; the session constructor splits by source (peer →
    /// child_leg_role, device → dial_leg_role).
    #[test]
    fn dial_leg_role_from_uui_marker() {
        let consult =
            parse_uui(&build_uui_value("self-call-id", None, None, None, Some("consult")))
                .unwrap();
        assert_eq!(consult.leg.as_deref(), Some("consult"));
        let transfer =
            parse_uui(&build_uui_value("self-call-id", None, None, None, Some("transfer")))
                .unwrap();
        assert_eq!(transfer.leg.as_deref(), Some("transfer"));
    }

    #[test]
    fn extract_skips_non_cc_uui_headers() {
        // An INVITE may carry several User-to-User headers; only the
        // purpose=call-center one belongs to the CC correlation.
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(Header::Other(
            UUI_HEADER_NAME.into(),
            "otherdata;encoding=hex;purpose=foo".into(),
        ));
        headers.push(Header::Other(
            UUI_HEADER_NAME.into(),
            build_uui_value("root-1", None, None, None, None),
        ));
        let uui = extract_cc_uui(&headers).expect("must find the CC header");
        assert_eq!(uui.session_id, "root-1");
    }

    #[test]
    fn header_name_match_is_case_insensitive() {
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(Header::Other(
            "user-to-user".into(),
            build_uui_value("root-2", None, None, None, None),
        ));
        let uui = extract_cc_uui(&headers).expect("lowercase header name must match");
        assert_eq!(uui.session_id, "root-2");
    }

    /// The inbound REFER transfer path stamps the target INVITE with exactly
    /// `build_uui_header(root, None, None, None, Some("transfer"))`; the
    /// inbound UAS leg must recover the root session id AND the leg role.
    #[test]
    fn transfer_target_uui_header_roundtrip() {
        let header = build_uui_header("root-transfer-1", None, None, None, Some("transfer"));
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(rsipstack::sip::Header::MaxForwards("70".into()));
        headers.push(header);
        let uui = extract_cc_uui(&headers).expect("transfer UUI must be extracted");
        assert_eq!(uui.session_id, "root-transfer-1");
        assert_eq!(uui.leg.as_deref(), Some("transfer"));
        assert!(uui.queue_id.is_none());
        assert!(uui.queue_name.is_none());
        assert!(uui.skill_group.is_none());
    }
}
