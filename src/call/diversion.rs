//! Call diversion bookkeeping: RFC 5806 `Diversion` / RFC 7044 `History-Info`.
//!
//! The dialplan accumulates a [`DiversionEntry`] chain: it starts from the
//! diversion headers of the inbound INVITE ([`parse_diversion_chain`]) and
//! grows by one entry per forwarding hop performed by this server
//! (unconditional user forwarding, call-forward-on-busy / no-answer).
//! When diversion headers are enabled for an outgoing leg the chain is
//! re-rendered ([`diversion_header`] / [`history_info_header`]), so chained
//! forwards never duplicate headers.
//!
//! Note: History-Info privacy (`np-di`, RFC 7246) is intentionally not
//! rendered; the `Diversion` header carries `privacy=` for peers that need it.

use rsipstack::sip::{Header, Request, uri::Uri};

/// Reason attached to a diversion hop (RFC 5806 §4 reason values).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiversionReason {
    Unknown,
    UserBusy,
    NoAnswer,
    Redirected,
    Unconditional,
    TimeOfDay,
    Deflection,
    FollowMe,
    OutOfService,
    Away,
}

impl DiversionReason {
    /// RFC 5806 wire value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::UserBusy => "user-busy",
            Self::NoAnswer => "no-answer",
            Self::Redirected => "redirected",
            Self::Unconditional => "unconditional",
            Self::TimeOfDay => "time-of-day",
            Self::Deflection => "deflection",
            Self::FollowMe => "follow-me",
            Self::OutOfService => "out-of-service",
            Self::Away => "away",
        }
    }

    /// Reason for a call-forward-on-failure hop triggered by a final SIP
    /// failure status. `None` when the status does not map to a standard
    /// diversion reason.
    pub fn from_failure_status(code: u16) -> Option<Self> {
        match code {
            486 | 600 | 603 => Some(Self::UserBusy),
            408 | 480 => Some(Self::NoAnswer),
            302 => Some(Self::Redirected),
            _ => None,
        }
    }

    /// `cause=` value carried in the History-Info Reason URI-header
    /// (RFC 7044 §5.2 / RFC 6431-style SIP cause codes).
    pub fn cause(&self) -> Option<u16> {
        match self {
            Self::UserBusy => Some(486),
            Self::NoAnswer => Some(408),
            Self::Redirected | Self::TimeOfDay | Self::Deflection | Self::FollowMe => Some(302),
            _ => None,
        }
    }

    fn cause_text(&self) -> Option<&'static str> {
        match self.cause() {
            Some(486) => Some("Busy Here"),
            Some(408) => Some("Request Timeout"),
            Some(302) => Some("Moved Temporarily"),
            _ => None,
        }
    }

    fn from_rsipstack(reason: &rsipstack::sip::typed::DiversionReason) -> Self {
        use rsipstack::sip::typed::DiversionReason as R;
        match reason {
            R::Unknown => Self::Unknown,
            R::UserBusy => Self::UserBusy,
            R::NoAnswer => Self::NoAnswer,
            R::Redirected => Self::Redirected,
            R::Unconditional => Self::Unconditional,
            R::TimeOfDay => Self::TimeOfDay,
            R::Deflection => Self::Deflection,
            R::FollowMe => Self::FollowMe,
            R::OutOfService => Self::OutOfService,
            R::Away => Self::Away,
            R::Other(_) => Self::Unknown,
        }
    }
}

impl std::fmt::Display for DiversionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// One diversion hop. The diverting party is the destination the call was
/// forwarded *from* (i.e. the previously dialed target), matching the
/// `Diversion` header semantics.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiversionEntry {
    /// Diverting party URI (plain string form, without angle brackets).
    pub uri: String,
    pub reason: Option<DiversionReason>,
    /// `counter` — redirections so far for this address, counted from 1.
    pub counter: u32,
    /// RFC 5806 privacy value: `full` / `off`.
    pub privacy: Option<String>,
}

impl DiversionEntry {
    /// A hop diverting from `uri` with the given reason; counter starts at 1.
    pub fn new(uri: impl Into<String>, reason: Option<DiversionReason>) -> Self {
        Self {
            uri: uri.into(),
            reason,
            counter: 1,
            privacy: None,
        }
    }
}

/// Percent-encode a Reason URI-header value for embedding in a hi-targeted-to
/// URI: `;` → `%3B`, `"` → `%22`, space → `%20` (RFC 7044 examples).
fn encode_reason_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            ';' => out.push_str("%3B"),
            '"' => out.push_str("%22"),
            ' ' => out.push_str("%20"),
            _ => out.push(ch),
        }
    }
    out
}

/// Percent-decode an ASCII string (`%xx` triplets → bytes); malformed
/// triplets pass through verbatim. Used for Reason URI-header values in
/// History-Info hi-targeted-to URIs, where the whole value (`;`, `"`, `=`,
/// spaces) is percent-encoded per the RFC 7044 examples.
fn decode_percent(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = if bytes[i] == b'%' {
            (bytes.get(i + 1).and_then(|h| (*h as char).to_digit(16)))
                .zip(bytes.get(i + 2).and_then(|l| (*l as char).to_digit(16)))
        } else {
            None
        };
        if let Some((hi, lo)) = hex {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reason_uri_header(reason: &DiversionReason) -> String {
    match (reason.cause(), reason.cause_text()) {
        (Some(cause), Some(text)) => format!(
            "Reason=SIP%3Bcause%3D{}%3Btext%3D%22{}%22",
            cause,
            encode_reason_value(text)
        ),
        _ => String::new(),
    }
}

/// Parse the diversion chain carried by an inbound request: RFC 5806
/// `Diversion` headers take precedence; when absent, RFC 7044 `History-Info`
/// hi-entries whose Reason URI-header carries a mappable `cause` are used.
pub fn parse_diversion_chain(req: &Request) -> Vec<DiversionEntry> {
    use rsipstack::sip::ToTypedHeader;
    use rsipstack::sip::prelude::HeadersExt;

    let mut chain = Vec::new();
    for header in req.diversion_headers() {
        let Ok(typed) = header.clone().into_typed() else {
            continue;
        };
        for entry in typed.entries {
            chain.push(DiversionEntry {
                uri: entry.uri.to_string(),
                reason: entry.reason.as_ref().map(DiversionReason::from_rsipstack),
                counter: entry.counter.unwrap_or(chain.len() as u32 + 1),
                privacy: entry.privacy.clone(),
            });
        }
    }
    if !chain.is_empty() {
        return chain;
    }
    let Ok(hi_entries) = req.history_info_entries() else {
        return chain;
    };
    for entry in hi_entries {
        let reason = entry.uri.headers.iter().find_map(|(name, value)| {
            if !name.eq_ignore_ascii_case("Reason") {
                return None;
            }
            // Wire form: `Reason=SIP%3Bcause%3D486%3Btext%3D%22Busy%20Here%22`.
            let decoded = decode_percent(value);
            let cause = decoded
                .split(';')
                .find_map(|p| p.trim().strip_prefix("cause="))
                .and_then(|c| c.trim().parse::<u16>().ok())?;
            DiversionReason::from_failure_status(cause)
        });
        chain.push(DiversionEntry {
            uri: entry.uri.to_string(),
            reason,
            counter: chain.len() as u32 + 1,
            privacy: None,
        });
    }
    chain
}

/// Render the chain as an RFC 5806 `Diversion` header (`None` when empty).
pub fn diversion_header(chain: &[DiversionEntry]) -> Option<Header> {
    if chain.is_empty() {
        return None;
    }
    let value = chain
        .iter()
        .map(|e| {
            let mut s = format!("<{}>", e.uri);
            if let Some(reason) = &e.reason {
                s.push_str(";reason=");
                s.push_str(reason.as_str());
            }
            s.push_str(&format!(";counter={}", e.counter));
            if let Some(privacy) = &e.privacy {
                s.push_str(";privacy=");
                s.push_str(privacy);
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(Header::Diversion(rsipstack::sip::headers::Diversion::new(
        value,
    )))
}

/// Render the chain as an RFC 7044 `History-Info` header, optionally closing
/// with a final hi-entry for the outgoing target (`None` when there is
/// nothing to render). Indices follow the hierarchical scheme: the first hop
/// is `1`, subsequent hops `1.1`, `1.2`, ... and the final target closes the
/// branch with the next index.
pub fn history_info_header(chain: &[DiversionEntry], final_target: Option<&Uri>) -> Option<Header> {
    if chain.is_empty() && final_target.is_none() {
        return None;
    }
    let index_of = |hop: usize| -> String {
        if hop == 0 {
            "1".to_string()
        } else {
            format!("1.{}", hop)
        }
    };
    let mut parts = Vec::new();
    for (hop, entry) in chain.iter().enumerate() {
        let mut part = format!("<{}", entry.uri);
        if let Some(reason) = &entry.reason {
            let header = reason_uri_header(reason);
            if !header.is_empty() {
                part.push('?');
                part.push_str(&header);
            }
        }
        part.push_str(&format!(">;index={}", index_of(hop)));
        parts.push(part);
    }
    if let Some(target) = final_target {
        let target_str = target.to_string();
        // Skip a final entry duplicating the last diverting party.
        if chain.last().map(|e| e.uri != target_str).unwrap_or(true) {
            parts.push(format!("<{}>;index={}", target_str, index_of(chain.len())));
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(Header::HistoryInfo(
        rsipstack::sip::headers::HistoryInfo::new(parts.join(", ")),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::sip::{Headers, Method, Version};

    fn invite_with(headers: &[(&str, &str)]) -> Request {
        let uri = rsipstack::sip::uri::parse_uri("sip:bob@example.com").unwrap();
        let mut req = Request {
            method: Method::Invite,
            uri,
            version: Version::V2,
            headers: Headers::default(),
            body: vec![],
        };
        for (name, value) in headers {
            // make_header mirrors the wire parse path: named headers become
            // their typed-untyped variants instead of Header::Other.
            req.headers.push(rsipstack::sip::headers::make_header(
                name,
                value.to_string(),
            ));
        }
        req
    }

    #[test]
    fn reason_mapping() {
        assert_eq!(
            DiversionReason::from_failure_status(486),
            Some(DiversionReason::UserBusy)
        );
        assert_eq!(
            DiversionReason::from_failure_status(603),
            Some(DiversionReason::UserBusy)
        );
        assert_eq!(
            DiversionReason::from_failure_status(408),
            Some(DiversionReason::NoAnswer)
        );
        assert_eq!(
            DiversionReason::from_failure_status(480),
            Some(DiversionReason::NoAnswer)
        );
        assert_eq!(
            DiversionReason::from_failure_status(302),
            Some(DiversionReason::Redirected)
        );
        assert_eq!(DiversionReason::from_failure_status(404), None);
        assert_eq!(DiversionReason::UserBusy.cause(), Some(486));
        assert_eq!(DiversionReason::Unconditional.cause(), None);
    }

    #[test]
    fn parse_diversion_header() {
        let req = invite_with(&[(
            "Diversion",
            "<sip:first@example.com>;reason=unconditional;counter=1, <sip:second@example.com>;reason=user-busy;counter=2;privacy=full",
        )]);
        let chain = parse_diversion_chain(&req);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].reason, Some(DiversionReason::Unconditional));
        assert_eq!(chain[0].counter, 1);
        assert_eq!(chain[1].reason, Some(DiversionReason::UserBusy));
        assert_eq!(chain[1].privacy.as_deref(), Some("full"));
    }

    #[test]
    fn parse_history_info_fallback() {
        let req = invite_with(&[(
            "History-Info",
            "<sip:first@example.com?Reason=SIP%3Bcause%3D486%3Btext%3D%22Busy%20Here%22>;index=1, <sip:second@example.com>;index=1.1",
        )]);
        let chain = parse_diversion_chain(&req);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].reason, Some(DiversionReason::UserBusy));
        assert_eq!(chain[1].reason, None);
    }

    #[test]
    fn diversion_header_roundtrip() {
        let chain = vec![
            DiversionEntry::new(
                "sip:first@example.com",
                Some(DiversionReason::Unconditional),
            ),
            {
                let mut e =
                    DiversionEntry::new("sip:second@example.com", Some(DiversionReason::NoAnswer));
                e.counter = 2;
                e.privacy = Some("full".into());
                e
            },
        ];
        let header = diversion_header(&chain).expect("header");
        let value = header.value().to_string();
        assert!(value.contains("<sip:first@example.com>;reason=unconditional;counter=1"));
        assert!(value.contains("<sip:second@example.com>;reason=no-answer;counter=2;privacy=full"));
        // Rendered value parses back into typed entries.
        let typed = rsipstack::sip::typed::Diversion::parse(&value).unwrap();
        assert_eq!(typed.entries.len(), 2);
    }

    #[test]
    fn history_info_renders_reason_and_final_target() {
        let chain = vec![DiversionEntry::new(
            "sip:first@example.com",
            Some(DiversionReason::UserBusy),
        )];
        let target = rsipstack::sip::uri::parse_uri("sip:final@example.com").unwrap();
        let header = history_info_header(&chain, Some(&target)).expect("header");
        let value = header.value().to_string();
        assert!(value.contains(
            "<sip:first@example.com?Reason=SIP%3Bcause%3D486%3Btext%3D%22Busy%20Here%22>;index=1"
        ));
        assert!(value.contains("<sip:final@example.com>;index=1.1"));
        // Parsed back through the rsipstack typed header.
        let parsed = history_info_header(&chain, Some(&target))
            .unwrap()
            .value()
            .to_string();
        let typed = rsipstack::sip::typed::HistoryInfo::parse(&parsed).unwrap();
        assert_eq!(typed.entries.len(), 2);
        assert_eq!(typed.entries[0].index, "1");
        assert_eq!(typed.entries[1].index, "1.1");
    }

    #[test]
    fn empty_chain_renders_nothing() {
        assert!(diversion_header(&[]).is_none());
        assert!(history_info_header(&[], None).is_none());
    }

    #[test]
    fn final_target_deduplicated() {
        let chain = vec![DiversionEntry::new("sip:final@example.com", None)];
        let target = rsipstack::sip::uri::parse_uri("sip:final@example.com").unwrap();
        let header = history_info_header(&chain, Some(&target)).expect("header");
        assert!(!header.value().contains("index=1.1"));
    }
}
