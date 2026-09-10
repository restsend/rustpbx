//! RFC 7989 global session-id resolution.
//!
//! Every logical call carries one global session id — a 32 hex char UUID
//! (the wire format of a SIP `Session-ID` local-uuid). It is inherited by
//! every child leg (queue dispatch, REFER transfer, consultation) and crosses
//! cluster nodes inside the `Session-ID` header, replacing the legacy UUI
//! (RFC 7433) correlation where available.
//!
//! Resolution order for an inbound INVITE:
//!
//! 1. `Session-ID` header local-uuid (upstream PBX/SBC already assigned one);
//! 2. the SIP Call-ID when it is UUID-shaped (exactly 32 hex chars, dashed
//!    RFC 4122 form is normalized);
//! 3. a freshly generated UUID v4 when the Call-ID is longer than a UUID or
//!    otherwise not UUID-shaped.
//!
//! Plain P2P extension-to-extension calls (no queue/ivr/app/forwarding) keep
//! the legacy behaviour: the Call-ID is used verbatim and no Session-ID is
//! generated or injected on the outgoing leg.

use rsipstack::sip::{HeadersExt, Request};

/// Generate a fresh global session id (UUID v4, 32 lowercase hex chars).
pub fn generate() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Normalize a candidate id to the canonical 32 lowercase hex form.
///
/// Accepts bare 32 hex chars and dashed RFC 4122 UUIDs. Returns `None` for
/// anything else (`Call-ID`s with a host part, longer/shorter ids, ...).
pub fn normalize(candidate: &str) -> Option<String> {
    rsipstack::sip::headers::SessionId::normalize(candidate).ok()
}

/// True when the candidate can serve as a global session id as-is.
pub fn is_uuid_like(candidate: &str) -> bool {
    normalize(candidate).is_some()
}

/// Resolve the global session id for an inbound request.
///
/// See the module docs for the resolution order. `fallback` is the SIP
/// Call-ID (or any caller-provided candidate).
pub fn resolve_incoming(request: &Request, fallback_call_id: &str) -> String {
    if let Some(header) = request.session_id_header()
        && let Some(local) = header.local_uuid()
    {
        return local;
    }
    normalize_or_generate(fallback_call_id)
}

/// Normalize a caller-provided id, generating a fresh UUID when it does not
/// fit the 32 hex char session-id format (e.g. Call-IDs longer than a UUID).
pub fn normalize_or_generate(candidate: &str) -> String {
    normalize(candidate).unwrap_or_else(generate)
}

/// Extract an upstream global session id from an inbound request's
/// `Session-ID` header local-uuid, when present.
///
/// Unlike [`resolve_incoming`] this never falls back to the Call-ID and
/// never generates a fresh id — used to re-attach an inbound leg to an
/// existing logical call (root inheritance).
pub fn extract_inherited(request: &Request) -> Option<String> {
    request
        .session_id_header()
        .and_then(|header| header.local_uuid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hex_is_normalized() {
        assert_eq!(
            normalize("0b9e6c1e1b404e8f9c212f5a8d3e7b61").as_deref(),
            Some("0b9e6c1e1b404e8f9c212f5a8d3e7b61")
        );
        assert_eq!(
            normalize("0B9E6C1E1B404E8F9C212F5A8D3E7B61").as_deref(),
            Some("0b9e6c1e1b404e8f9c212f5a8d3e7b61")
        );
    }

    #[test]
    fn dashed_uuid_is_normalized() {
        assert_eq!(
            normalize("0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61").as_deref(),
            Some("0b9e6c1e1b404e8f9c212f5a8d3e7b61")
        );
    }

    #[test]
    fn callid_with_host_is_rejected() {
        // rsipstack default Call-ID format: <uuid>@suffix — longer than a UUID.
        assert!(normalize("0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61@restsend.com").is_none());
    }

    #[test]
    fn overlong_id_is_rejected() {
        assert!(normalize("0b9e6c1e1b404e8f9c212f5a8d3e7b61ff").is_none());
        assert!(normalize("bleg-1001-1-0b9e6c1e1b404e8f").is_none());
    }

    #[test]
    fn short_or_garbage_is_rejected() {
        assert!(normalize("").is_none());
        assert!(normalize("1001").is_none());
        assert!(normalize("urn:uuid:not-a-uuid").is_none());
    }

    #[test]
    fn generated_is_uuid_like() {
        let id = generate();
        assert_eq!(id.len(), 32);
        assert!(is_uuid_like(&id));
    }

    #[test]
    fn normalize_or_generate_keeps_valid_and_replaces_invalid() {
        assert_eq!(
            normalize_or_generate("0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61"),
            "0b9e6c1e1b404e8f9c212f5a8d3e7b61"
        );
        let generated = normalize_or_generate("session-with-host@pbx.example");
        assert!(is_uuid_like(&generated));
        assert_ne!(generated, "session-with-host@pbx.example");
    }

    fn invite(headers: rsipstack::sip::Headers) -> Request {
        Request {
            method: rsipstack::sip::Method::Invite,
            uri: "sip:b@pbx".parse().unwrap(),
            version: rsipstack::sip::Version::V2,
            headers,
            body: Vec::new(),
        }
    }

    #[test]
    fn resolve_prefers_session_id_header_local_uuid() {
        let local = rsipstack::sip::headers::SessionId::normalize(
            "aabbccddeeff00112233445566778899",
        )
        .unwrap();
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(
            rsipstack::sip::headers::SessionId::from_local(&local)
                .unwrap()
                .into(),
        );
        assert_eq!(
            resolve_incoming(&invite(headers), "0b9e6c1e1b404e8f9c212f5a8d3e7b61"),
            "aabbccddeeff00112233445566778899"
        );
    }

    #[test]
    fn resolve_falls_back_to_uuid_like_call_id() {
        assert_eq!(
            resolve_incoming(&invite(rsipstack::sip::Headers::default()), "0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61"),
            "0b9e6c1e1b404e8f9c212f5a8d3e7b61"
        );
    }

    #[test]
    fn resolve_generates_for_overlong_call_id() {
        let id = resolve_incoming(
            &invite(rsipstack::sip::Headers::default()),
            "0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61@restsend.com",
        );
        assert!(is_uuid_like(&id));
    }
}
