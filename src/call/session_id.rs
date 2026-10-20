//! Global session-id resolution and generation.
//!
//! Every logical call carries one global session id — a lowercase hex string.
//! Freshly generated ids are [`settings()`](SessionIdSettings) long
//! (`proxy.session_id_length`, default **32**: the historical UUID v4 simple
//! form; shorter ids such as 16 are opt-in). Inherited ids — upstream
//! `Session-ID` headers, UUID-shaped Call-IDs, CC-UUI data — are kept
//! verbatim, so 32-hex and short ids coexist in the wild.
//!
//! Resolution order for an inbound INVITE:
//!
//! 1. `Session-ID` header local part (upstream PBX/SBC already assigned one);
//! 2. the SIP Call-ID when it is id-shaped (32 hex chars or the configured
//!    length; dashed RFC 4122 form is normalized) — unless
//!    `session_id_inherit_callid` is disabled;
//! 3. a freshly generated random id.
//!
//! Plain P2P extension-to-extension calls (no queue/ivr/app/forwarding) keep
//! the legacy behaviour: the Call-ID is used verbatim and no Session-ID is
//! generated or injected on the outgoing leg.
//!
//! Wire format of the outgoing `Session-ID` header is configurable
//! (`proxy.session_id_header_mode`): `native` stamps the internal id verbatim
//! (short ids are not strictly RFC 7989 shaped — strict peers discard the
//! header and fall back to Call-ID correlation), `rfc7989` zero-pads the id
//! to 32 hex chars so the header always satisfies the RFC.

use std::sync::RwLock;

use rsipstack::sip::{Header, HeadersExt, Request};

/// Outgoing `Session-ID` header wire format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionIdHeaderMode {
    /// Stamp the internal id verbatim (`local;remote=NIL`). Ids shorter than
    /// 32 hex chars bypass rsipstack's RFC 7989 validation and are carried as
    /// a raw header; strict peers ignore the header (graceful degradation to
    /// Call-ID correlation).
    #[default]
    Native,
    /// Zero-pad the id on the left to 32 hex chars so the header always
    /// satisfies RFC 7989's UUID shape.
    Rfc7989,
}

/// Session-id generator settings, seeded from `[proxy]` config at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionIdSettings {
    /// Length of freshly generated ids in hex chars (8..=32).
    pub length: usize,
    /// Wire format of the outgoing `Session-ID` header.
    pub header_mode: SessionIdHeaderMode,
    /// Reuse id-shaped incoming Call-IDs as session ids.
    pub inherit_callid: bool,
}

impl Default for SessionIdSettings {
    fn default() -> Self {
        // Defaults mirror the historical behaviour exactly: 32-hex ids
        // (UUID v4 simple), native headers, Call-ID reuse enabled.
        Self {
            length: 32,
            header_mode: SessionIdHeaderMode::Native,
            inherit_callid: true,
        }
    }
}

/// Legal bounds for [`SessionIdSettings::length`].
pub const LENGTH_MIN: usize = 8;
pub const LENGTH_MAX: usize = 32;

static SETTINGS: RwLock<SessionIdSettings> = RwLock::new(SessionIdSettings {
    length: 32,
    header_mode: SessionIdHeaderMode::Native,
    inherit_callid: true,
});

/// Install startup settings. Invalid lengths are clamped into
/// `[LENGTH_MIN, LENGTH_MAX]`; the effective value is returned.
pub fn init_settings(
    length: usize,
    header_mode: SessionIdHeaderMode,
    inherit_callid: bool,
) -> usize {
    let length = length.clamp(LENGTH_MIN, LENGTH_MAX);
    if let Ok(mut settings) = SETTINGS.write() {
        *settings = SessionIdSettings {
            length,
            header_mode,
            inherit_callid,
        };
    }
    length
}

/// Current settings (a cheap copy; generation is per-call, not per-packet).
pub fn settings() -> SessionIdSettings {
    SETTINGS.read().map(|s| *s).unwrap_or_default()
}

/// Generate a fresh global session id (`settings().length` random hex chars).
pub fn generate() -> String {
    generate_with(settings().length)
}

/// Generate a fresh id of exactly `length` random lowercase hex chars.
///
/// Drawn from a uniform 128-bit random value — unlike a truncated UUID v4
/// there is no fixed version/variant nibble at predictable positions.
pub fn generate_with(length: usize) -> String {
    let length = length.clamp(LENGTH_MIN, LENGTH_MAX);
    format!("{:032x}", rand::random::<u128>())[..length].to_string()
}

/// Normalize a candidate id to its canonical lowercase hex form.
///
/// Accepts bare 32 hex chars, ids of the configured length, and dashed
/// RFC 4122 UUIDs. Returns `None` for anything else (`Call-ID`s with a host
/// part, other lengths, all-zero (nil) values, garbage, ...).
pub fn normalize(candidate: &str) -> Option<String> {
    normalize_with(candidate, settings().length)
}

/// [`normalize`] against an explicit length (unit-testable variant).
pub fn normalize_with(candidate: &str, length: usize) -> Option<String> {
    let v: String = candidate
        .trim()
        .trim_start_matches("urn:uuid:")
        .replace('-', "")
        .to_ascii_lowercase();
    if v.len() != LENGTH_MAX && v.len() != length.clamp(LENGTH_MIN, LENGTH_MAX) {
        return None;
    }
    if !v.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Nil semantics at any accepted length (RFC 7989 §5 reserves all-zeros).
    if v.bytes().all(|b| b == b'0') {
        return None;
    }
    Some(v)
}

/// True when the candidate can serve as a global session id as-is.
pub fn is_session_id_like(candidate: &str) -> bool {
    normalize(candidate).is_some()
}

/// Resolve the global session id for an inbound request.
///
/// See the module docs for the resolution order. `fallback` is the SIP
/// Call-ID (or any caller-provided candidate).
pub fn resolve_incoming(request: &Request, fallback_call_id: &str) -> String {
    if let Some(local) = extract_inherited(request) {
        return local;
    }
    normalize_or_generate(fallback_call_id)
}

/// Derive the session id for a call we originate, honouring
/// `session_id_inherit_callid`: when disabled the caller-provided id is never
/// reused and a fresh id is generated (the Call-ID keeps its wire role only).
pub fn derive_from_call_id(candidate: &str) -> String {
    if settings().inherit_callid {
        normalize_or_generate(candidate)
    } else {
        generate()
    }
}

/// Normalize a caller-provided id, generating a fresh id when it does not fit
/// the id format (e.g. Call-IDs longer than a UUID).
pub fn normalize_or_generate(candidate: &str) -> String {
    normalize(candidate).unwrap_or_else(generate)
}

/// Extract an upstream global session id from an inbound request's
/// `Session-ID` header, when present.
///
/// Unlike [`resolve_incoming`] this never falls back to the Call-ID and
/// never generates a fresh id — used to re-attach an inbound leg to an
/// existing logical call (root inheritance).
pub fn extract_inherited(request: &Request) -> Option<String> {
    request
        .session_id_header()
        .and_then(|header| parse_wire_header(header.value()))
}

/// Extract the local id from a raw `Session-ID` header value
/// (`local;remote=...` or the legacy `local;remote` form).
///
/// rsipstack's `SessionId::local_uuid()` only recognizes 32-hex values; this
/// parser is deliberately liberal (any hex id in `[LENGTH_MIN, LENGTH_MAX]`)
/// so natively stamped short ids round-trip between nodes configured with
/// different lengths. The header is an explicit correlation channel — unlike
/// Call-ID reuse there is no accidental-shape risk in accepting it.
pub fn parse_wire_header(raw: &str) -> Option<String> {
    let local = raw.split(';').next().unwrap_or("").trim();
    normalize_inherited(local)
}

/// Validate an explicitly inherited id independently of the local generator
/// length, so forwarding across differently configured nodes preserves it.
pub fn normalize_inherited(candidate: &str) -> Option<String> {
    let v: String = candidate
        .trim()
        .trim_start_matches("urn:uuid:")
        .replace('-', "")
        .to_ascii_lowercase();
    if !v.len().ge(&LENGTH_MIN) || !v.len().le(&LENGTH_MAX) {
        return None;
    }
    if !v.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // Nil semantics at any accepted length (RFC 7989 §5 reserves all-zeros).
    if v.bytes().all(|b| b == b'0') {
        return None;
    }
    Some(v)
}

/// Wire representation for stamping a session id on an outgoing INVITE.
///
/// Returns `(InviteOption.session_id, extra_header)` — exactly one side is
/// set. Ids that satisfy rsipstack's 32-hex validation (plain 32-hex, or any
/// id under `rfc7989` mode after zero-padding) ride the regular
/// `InviteOption.session_id` field; native-mode short ids cannot pass that
/// validation (`invitation.rs` rejects the whole request), so they are
/// carried as a raw `Session-ID` header in `InviteOption.headers` instead
/// (`SessionId::new` does not validate).
pub fn build_wire_header(id: Option<&str>) -> (Option<String>, Option<Header>) {
    let Some(id) = id else {
        return (None, None);
    };
    let Some(clean) = normalize_inherited(id) else {
        // Defensive: never stamp a non-id-shaped candidate on the wire.
        return (None, None);
    };
    if clean.len() == LENGTH_MAX {
        return (Some(clean), None);
    }
    match settings().header_mode {
        SessionIdHeaderMode::Rfc7989 => {
            // Zero-pad on the left: deterministic, reversible shape, and
            // rsipstack accepts the result without any bypass.
            (Some(format!("{:0>32}", clean)), None)
        }
        SessionIdHeaderMode::Native => {
            use rsipstack::sip::headers::SessionId;
            let header = Header::from(SessionId::new(format!(
                "{};remote={}",
                clean,
                SessionId::NIL
            )));
            (None, Some(header))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hex_is_normalized() {
        assert_eq!(
            normalize_with("0b9e6c1e1b404e8f9c212f5a8d3e7b61", 32).as_deref(),
            Some("0b9e6c1e1b404e8f9c212f5a8d3e7b61")
        );
        assert_eq!(
            normalize_with("0B9E6C1E1B404E8F9C212F5A8D3E7B61", 32).as_deref(),
            Some("0b9e6c1e1b404e8f9c212f5a8d3e7b61")
        );
    }

    #[test]
    fn dashed_uuid_is_normalized() {
        assert_eq!(
            normalize_with("0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61", 32).as_deref(),
            Some("0b9e6c1e1b404e8f9c212f5a8d3e7b61")
        );
    }

    #[test]
    fn short_id_of_configured_length_is_normalized() {
        assert_eq!(
            normalize_with("0b9e6c1e1b404e8f", 16).as_deref(),
            Some("0b9e6c1e1b404e8f")
        );
        // The same value is rejected when the configured length differs.
        assert_eq!(normalize_with("0b9e6c1e1b404e8f", 32), None);
    }

    #[test]
    fn callid_with_host_is_rejected() {
        // rsipstack default Call-ID format: <uuid>@suffix — longer than a UUID.
        assert_eq!(
            normalize_with("0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61@restsend.com", 32),
            None
        );
    }

    #[test]
    fn overlong_id_is_rejected() {
        assert_eq!(normalize_with("0b9e6c1e1b404e8f9c212f5a8d3e7b61ff", 32), None);
        assert_eq!(normalize_with("bleg-1001-1-0b9e6c1e1b404e8f", 16), None);
    }

    #[test]
    fn short_or_garbage_or_nil_is_rejected() {
        assert_eq!(normalize_with("", 16), None);
        assert_eq!(normalize_with("1001", 16), None);
        assert_eq!(normalize_with("urn:uuid:not-a-uuid", 32), None);
        // Off-by-one lengths and all-zero (nil) values never normalize.
        assert_eq!(normalize_with("0b9e6c1e1b404e8", 16), None);
        assert_eq!(normalize_with("0b9e6c1e1b404e8ff", 16), None);
        assert_eq!(normalize_with("0000000000000000", 16), None);
        assert_eq!(
            normalize_with("00000000-0000-0000-0000-000000000000", 32),
            None
        );
    }

    #[test]
    fn generated_matches_length() {
        let id = generate_with(32);
        assert_eq!(id.len(), 32);
        assert!(is_session_id_like(&id));

        let short = generate_with(16);
        assert_eq!(short.len(), 16);
        assert!(short.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(normalize_with(&short, 16).is_some());
        // Uniform nibbles: no structural constraint (a random char may
        // legitimately be '4' — the point is it is not FIXED at position 12).
        assert!(normalize_with(&short, 32).is_none());
    }

    #[test]
    fn generate_is_uniformly_shaped_across_lengths() {
        for length in [8, 12, 16, 24, 32] {
            let id = generate_with(length);
            assert_eq!(id.len(), length);
            assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(normalize_with(&id, length).is_some());
        }
    }

    #[test]
    fn normalize_or_generate_keeps_valid_and_replaces_invalid() {
        assert_eq!(
            normalize_or_generate("0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61"),
            "0b9e6c1e1b404e8f9c212f5a8d3e7b61"
        );
        let generated = normalize_or_generate("session-with-host@pbx.example");
        assert!(generated.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!((LENGTH_MIN..=LENGTH_MAX).contains(&generated.len()));
        assert_ne!(generated, "session-with-host@pbx.example");
    }

    /// Swap the global settings for the duration of `f`, restoring before
    /// returning. Assertions in other tests stay settings-agnostic so brief
    /// interleavings under the parallel test runner stay harmless.
    fn with_settings<T>(
        length: usize,
        header_mode: SessionIdHeaderMode,
        inherit_callid: bool,
        f: impl FnOnce() -> T,
    ) -> T {
        let prev = settings();
        init_settings(length, header_mode, inherit_callid);
        let out = f();
        init_settings(prev.length, prev.header_mode, prev.inherit_callid);
        out
    }

    #[test]
    fn derive_from_call_id_honours_inherit_switch() {
        // Default settings inherit_callid == true: id-shaped Call-IDs reused.
        assert_eq!(
            derive_from_call_id("0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61"),
            "0b9e6c1e1b404e8f9c212f5a8d3e7b61"
        );
        // Disabled: every originate gets a fresh id, the Call-ID keeps only
        // its wire role.
        with_settings(16, SessionIdHeaderMode::Native, false, || {
            let derived = derive_from_call_id("0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61");
            assert_eq!(derived.len(), 16);
            assert_ne!(derived, "0b9e6c1e1b404e8f9c212f5a8d3e7b61");
        });
    }

    #[test]
    fn wire_header_32_hex_rides_invite_option() {
        let id = "0b9e6c1e1b404e8f9c212f5a8d3e7b61";
        let (session_id, header) = build_wire_header(Some(id));
        assert_eq!(session_id.as_deref(), Some(id));
        assert!(header.is_none());
    }

    #[test]
    fn wire_header_rfc7989_pads_short_ids() {
        let id = "0b9e6c1e1b404e8f";
        let padded = format!("{:0>32}", id);
        with_settings(16, SessionIdHeaderMode::Rfc7989, true, || {
            let (session_id, header) = build_wire_header(Some(id));
            // Rides the regular InviteOption field, zero-padded to 32 hex —
            // rsipstack accepts it without any bypass.
            assert_eq!(session_id.as_deref(), Some(padded.as_str()));
            assert!(header.is_none());
            assert_eq!(
                normalize_with(&padded, 32).as_deref(),
                Some(padded.as_str())
            );
        });
    }

    #[test]
    fn wire_header_native_short_id_bypasses_via_raw_header() {
        // Native mode + a 16-hex id: the id cannot pass rsipstack validation,
        // so it must ride the raw-header channel.
        let id = "0b9e6c1e1b404e8f";
        let (session_id, header) = with_settings(16, SessionIdHeaderMode::Native, true, || {
            build_wire_header(Some(id))
        });
        assert_eq!(session_id, None);
        let Some(Header::SessionId(sid)) = header else {
            panic!("expected a raw Session-ID header");
        };
        assert_eq!(
            sid.value(),
            &format!("{id};remote={}", rsipstack::sip::headers::SessionId::NIL)
        );
        // The raw header round-trips through our own liberal parser.
        assert_eq!(parse_wire_header(sid.value()).as_deref(), Some(id));
    }

    #[test]
    fn wire_header_none_and_garbage_are_noops() {
        assert_eq!(build_wire_header(None), (None, None));
        assert_eq!(build_wire_header(Some("junk@host")), (None, None));
    }

    #[test]
    fn parse_wire_header_handles_params_and_legacy_form() {
        assert_eq!(
            parse_wire_header("0b9e6c1e1b404e8f;remote=00000000000000000000000000000000")
                .as_deref(),
            Some("0b9e6c1e1b404e8f")
        );
        assert_eq!(
            parse_wire_header("0b9e6c1e1b404e8f9c212f5a8d3e7b61;remote=0b9e6c1e1b404e8f")
                .as_deref(),
            Some("0b9e6c1e1b404e8f9c212f5a8d3e7b61")
        );
        assert_eq!(parse_wire_header("0000000000000000;remote=x"), None);
        assert_eq!(parse_wire_header(""), None);
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
    fn resolve_prefers_session_id_header() {
        let mut headers = rsipstack::sip::Headers::default();
        headers.push(
            rsipstack::sip::headers::SessionId::from_local("aabbccddeeff00112233445566778899")
                .unwrap()
                .into(),
        );
        assert_eq!(
            resolve_incoming(
                &invite(headers),
                "0b9e6c1e1b404e8f9c212f5a8d3e7b61"
            ),
            "aabbccddeeff00112233445566778899"
        );
    }

    #[test]
    fn resolve_accepts_short_native_session_id_header() {
        let mut headers = rsipstack::sip::Headers::default();
        // Natively stamped short id: rsipstack's local_uuid() would ignore
        // this header; our parser must accept it.
        headers.push(
            rsipstack::sip::headers::SessionId::new("aabbccddeeff0011;remote=00000000000000000000000000000000")
                .into(),
        );
        assert_eq!(
            resolve_incoming(
                &invite(headers),
                "0b9e6c1e1b404e8f9c212f5a8d3e7b61"
            ),
            "aabbccddeeff0011"
        );
    }

    #[test]
    fn resolve_falls_back_to_uuid_like_call_id() {
        assert_eq!(
            resolve_incoming(
                &invite(rsipstack::sip::Headers::default()),
                "0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61"
            ),
            "0b9e6c1e1b404e8f9c212f5a8d3e7b61"
        );
    }

    #[test]
    fn resolve_generates_for_overlong_call_id() {
        let id = resolve_incoming(
            &invite(rsipstack::sip::Headers::default()),
            "0b9e6c1e-1b40-4e8f-9c21-2f5a8d3e7b61@restsend.com",
        );
        // Settings-agnostic: hex-only, non-nil, within the legal length band.
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!((LENGTH_MIN..=LENGTH_MAX).contains(&id.len()));
        assert!(id.bytes().any(|b| b != b'0'));
    }
}
