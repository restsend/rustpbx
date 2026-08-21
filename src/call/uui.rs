//! RFC 7433 SIP User-to-User (UUI) header handling for call-center session
//! correlation.
//!
//! Line format used by rustpbx (CC addon):
//!
//! ```text
//! User-to-User: <session_id>;encoding=hex;purpose=call-center;queue=<id>;qn=<urlencoded-name>;skill=<group>
//! ```
//!
//! - `session_id` — the root session identifier (first INVITE Call-ID of the
//!   call, or a generated root id for originates). Constant across all child
//!   legs (queue dispatch, REFER transfers, consultations).
//! - `purpose=call-center` — the RFC 7433 registered purpose for call-center
//!   data. Headers without this purpose are ignored by the CC integration.
//! - `queue` / `qn` / `skill` — optional CC context (queue canonical key,
//!   human-readable queue name, skill-group id).
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

/// Build a `User-to-User` header value for the CC purpose.
pub fn build_uui_value(
    session_id: &str,
    queue_id: Option<&str>,
    queue_name: Option<&str>,
    skill_group: Option<&str>,
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
    value
}

/// Build the `User-to-User` SIP header.
pub fn build_uui_header(
    session_id: &str,
    queue_id: Option<&str>,
    queue_name: Option<&str>,
    skill_group: Option<&str>,
) -> Header {
    Header::Other(
        UUI_HEADER_NAME.to_string(),
        build_uui_value(session_id, queue_id, queue_name, skill_group),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip_full() {
        let value = build_uui_value("abc123", Some("sales"), Some("Sales Hotline"), Some("sg-1"));
        let uui = parse_uui(&value).expect("must parse");
        assert_eq!(uui.session_id, "abc123");
        assert_eq!(uui.queue_id.as_deref(), Some("sales"));
        assert_eq!(uui.queue_name.as_deref(), Some("Sales Hotline"));
        assert_eq!(uui.skill_group.as_deref(), Some("sg-1"));
    }

    #[test]
    fn parse_minimal() {
        let uui = parse_uui("deadbeef;encoding=hex;purpose=call-center").unwrap();
        assert_eq!(uui.session_id, "deadbeef");
        assert!(uui.queue_id.is_none());
        assert!(uui.queue_name.is_none());
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
        let value = build_uui_value("s1", None, Some("支持中心 中文"), None);
        let uui = parse_uui(&value).unwrap();
        assert_eq!(uui.queue_name.as_deref(), Some("支持中心 中文"));
    }

    #[test]
    fn extract_from_headers() {
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(Header::Other("X-Ignored".into(), "1".into()));
        headers.push(Header::Other(
            UUI_HEADER_NAME.into(),
            build_uui_value("sid-1", Some("q1"), None, None),
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
            build_uui_value("root-1", None, None, None),
        ));
        let uui = extract_cc_uui(&headers).expect("must find the CC header");
        assert_eq!(uui.session_id, "root-1");
    }

    #[test]
    fn header_name_match_is_case_insensitive() {
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(Header::Other(
            "user-to-user".into(),
            build_uui_value("root-2", None, None, None),
        ));
        let uui = extract_cc_uui(&headers).expect("lowercase header name must match");
        assert_eq!(uui.session_id, "root-2");
    }

    /// The inbound REFER transfer path stamps the target INVITE with exactly
    /// `build_uui_header(root, None, None, None)`; the inbound UAS leg must
    /// recover the same root session id from it.
    #[test]
    fn transfer_target_uui_header_roundtrip() {
        let header = build_uui_header("root-transfer-1", None, None, None);
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(rsipstack::sip::Header::MaxForwards("70".into()));
        headers.push(header);
        let uui = extract_cc_uui(&headers).expect("transfer UUI must be extracted");
        assert_eq!(uui.session_id, "root-transfer-1");
        assert!(uui.queue_id.is_none());
        assert!(uui.queue_name.is_none());
        assert!(uui.skill_group.is_none());
    }
}
