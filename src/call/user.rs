use anyhow::Result;
use rsipstack::sip::{
    Header, Transport,
    headers::auth::Algorithm,
    prelude::{HeadersExt, ToTypedHeader},
    typed::Authorization,
};
use rsipstack::{
    transaction::transaction::Transaction,
    transport::{SipAddr, SipConnection},
};
use serde::{Deserialize, Serialize};

use super::{CallForwardingConfig, CallForwardingMode, TransferEndpoint};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SipUser {
    #[serde(default)]
    pub id: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub username: String,
    pub password: Option<String>,
    pub realm: Option<String>,
    pub departments: Option<Vec<String>>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub note: Option<String>,
    #[serde(default)]
    pub allow_guest_calls: bool,
    /// When `true` the extension has opted out of voicemail; unanswered calls
    /// should **not** be forwarded to the voicemail application.
    #[serde(default)]
    pub voicemail_disabled: bool,
    #[serde(default)]
    pub call_forwarding_mode: Option<String>,
    #[serde(default)]
    pub call_forwarding_destination: Option<String>,
    #[serde(default)]
    pub call_forwarding_timeout: Option<i32>,
    /// From the original INVITE
    #[serde(skip)]
    pub origin_contact: Option<rsipstack::sip::typed::Contact>,
    /// Current contact (may be updated by REGISTER)
    #[serde(skip)]
    pub contact: Option<rsipstack::sip::typed::Contact>,
    #[serde(skip)]
    pub from: Option<rsipstack::sip::Uri>,
    #[serde(skip)]
    pub destination: Option<SipAddr>,
    #[serde(default = "default_is_support_webrtc")]
    pub is_support_webrtc: bool,
}

impl std::fmt::Display for SipUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(realm) = &self.realm {
            write!(f, "{}@{}", self.username, realm)
        } else {
            write!(f, "{}", self.username)
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_is_support_webrtc() -> bool {
    false
}

impl Default for SipUser {
    fn default() -> Self {
        Self {
            id: 0,
            enabled: true,
            username: "".to_string(),
            password: None,
            realm: None,
            origin_contact: None,
            contact: None,
            from: None,
            destination: None,
            is_support_webrtc: false,
            departments: None,
            display_name: None,
            email: None,
            phone: None,
            note: None,
            allow_guest_calls: false,
            voicemail_disabled: false,
            call_forwarding_mode: None,
            call_forwarding_destination: None,
            call_forwarding_timeout: None,
        }
    }
}

impl SipUser {
    pub fn get_contact_username(&self) -> String {
        match self.origin_contact {
            Some(ref contact) => contact.uri.user().unwrap_or_default().to_string(),
            None => self.username.clone(),
        }
    }
    pub fn merge_with(&mut self, other: &SipUser) {
        if self.id == 0 {
            self.id = other.id;
        }
        if self.password.is_none() {
            self.password = other.password.clone();
        }
        if self.realm.is_none() {
            self.realm = other.realm.clone();
        }
        if self.departments.is_none() {
            self.departments = other.departments.clone();
        }
        if self.display_name.is_none() {
            self.display_name = other.display_name.clone();
        }
        if self.email.is_none() {
            self.email = other.email.clone();
        }
        if self.phone.is_none() {
            self.phone = other.phone.clone();
        }
        if self.note.is_none() {
            self.note = other.note.clone();
        }
        if !self.allow_guest_calls {
            self.allow_guest_calls = other.allow_guest_calls;
        }
        if self.origin_contact.is_none() {
            self.origin_contact = other.origin_contact.clone();
        }
        if self.contact.is_none() {
            self.contact = other.contact.clone();
        }
        if self.from.is_none() {
            self.from = other.from.clone();
        }
        if self.destination.is_none() {
            self.destination = other.destination.clone();
        }
        if !self.is_support_webrtc {
            self.is_support_webrtc = other.is_support_webrtc;
        }
    }

    pub fn forwarding_config(&self) -> Option<CallForwardingConfig> {
        let mode_text = self
            .call_forwarding_mode
            .as_deref()
            .map(|value| value.trim().to_lowercase())?;
        if mode_text.is_empty() || mode_text == "none" {
            return None;
        }

        let destination = self
            .call_forwarding_destination
            .as_deref()
            .map(|value| value.trim())?;
        if destination.is_empty() {
            return None;
        }

        let endpoint = TransferEndpoint::parse(destination)?;

        let mode = match mode_text.as_str() {
            "always" => CallForwardingMode::Always,
            "when_busy" | "busy" => CallForwardingMode::WhenBusy,
            "when_not_answered" | "no_answer" => CallForwardingMode::WhenNoAnswer,
            _ => return None,
        };

        let timeout_secs = self
            .call_forwarding_timeout
            .map(|value| CallForwardingConfig::clamp_timeout(value as i64))
            .unwrap_or(super::CALL_FORWARDING_TIMEOUT_DEFAULT_SECS);

        Some(CallForwardingConfig::new(mode, endpoint, timeout_secs))
    }

    fn build_contact(&mut self, tx: &Transaction) {
        let addr = match tx.endpoint_inner.get_addrs().first() {
            Some(addr) => addr.clone(),
            None => return,
        };

        let mut contact_params = vec![];
        match addr.r#type {
            Some(rsipstack::sip::Transport::Udp) | None => {}
            Some(t) => {
                contact_params.push(rsipstack::sip::Param::Transport(t));
            }
        }
        let contact = rsipstack::sip::typed::Contact {
            display_name: None,
            uri: rsipstack::sip::Uri {
                scheme: addr.r#type.map(|t| t.sip_scheme()),
                auth: Some(rsipstack::sip::Auth {
                    user: self.get_contact_username(),
                    password: None,
                }),
                host_with_port: addr.addr.clone(),
                ..Default::default()
            },
            params: contact_params,
        };
        self.contact = Some(contact);
    }

    pub fn auth_digest(&self, algorithm: Algorithm) -> String {
        use md5::{Digest, Md5};
        use sha2::{Sha256, Sha512};
        let value = format!(
            "{}:{}:{}",
            self.username,
            self.realm.as_ref().unwrap_or(&"".to_string()),
            self.password.as_ref().unwrap_or(&"".to_string()),
        );
        fn to_hex(bytes: impl AsRef<[u8]>) -> String {
            bytes
                .as_ref()
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect()
        }
        match algorithm {
            Algorithm::Md5 | Algorithm::Md5Sess => {
                let mut hasher = Md5::new();
                hasher.update(value);
                to_hex(hasher.finalize())
            }
            Algorithm::Sha256 | Algorithm::Sha256Sess => {
                let mut hasher = Sha256::new();
                hasher.update(value);
                to_hex(hasher.finalize())
            }
            Algorithm::Sha512 | Algorithm::Sha512Sess => {
                let mut hasher = Sha512::new();
                hasher.update(value);
                to_hex(hasher.finalize())
            }
        }
    }
}

impl TryFrom<&Transaction> for SipUser {
    type Error = anyhow::Error;

    fn try_from(tx: &Transaction) -> Result<Self, Self::Error> {
        let from_header = tx.original.from_header()?;
        let from_uri = from_header.uri()?;
        let from_display_name = from_header
            .typed()
            .ok()
            .and_then(|h| h.display_name)
            .map(|s| s.to_string());

        let (username, realm) = match check_authorization_headers(&tx.original) {
            Ok(Some((user, _))) => (user.username, user.realm),
            _ => {
                let username = from_uri.user().unwrap_or_default().to_string();
                let realm = from_uri.host().to_string();
                let realm = if let Some(port) = from_uri.host_with_port.port {
                    Some(format!("{}:{}", realm, port))
                } else {
                    Some(realm)
                };
                (username, realm)
            }
        };

        let origin_contact = match tx.original.contact_header() {
            Ok(contact) => contact.typed().ok(),
            Err(_) => None,
        };
        // Use rsipstack's via_received functionality to get destination
        let via_header = tx.original.via_header()?;
        let (via_transport, destination_addr) = SipConnection::parse_target_from_via(via_header)
            .map_err(|e| anyhow::anyhow!("failed to parse via header: {:?}", e))?;

        let mut destination = SipAddr {
            r#type: Some(via_transport),
            addr: destination_addr,
        };

        apply_flow_destination(&mut destination, tx.connection.as_ref());

        let is_support_webrtc = matches!(via_transport, Transport::Wss | Transport::Ws);

        let mut u = SipUser {
            id: 0,
            username,
            password: None,
            enabled: true,
            realm,
            origin_contact,
            contact: None,
            from: Some(from_uri),
            destination: Some(destination),
            is_support_webrtc,
            call_forwarding_mode: None,
            call_forwarding_destination: None,
            call_forwarding_timeout: None,
            departments: None,
            display_name: from_display_name,
            email: None,
            phone: None,
            note: None,
            allow_guest_calls: false,
            voicemail_disabled: false,
        };
        u.build_contact(tx);
        Ok(u)
    }
}

/// RFC 5626 / RFC 7118: for reliable transports the connection that actually
/// delivered the request is the authoritative source address. Browser clients
/// advertise `.invalid` flow tokens (or NAT-stale hosts) in their Via
/// sent-by, and received/rport stamping is not guaranteed on every transport
/// path (e.g. binary WebSocket frames). Prefer the concrete peer address of
/// the flow while keeping the Via-derived transport tag (Channel connections
/// report UDP regardless of the underlying WebSocket).
fn apply_flow_destination(destination: &mut SipAddr, connection: Option<&SipConnection>) {
    let Some(connection) = connection else {
        return;
    };
    if !connection.is_reliable() {
        return;
    }
    let Some(remote) = connection.get_remote_addr() else {
        return;
    };
    if matches!(&remote.addr.host, rsipstack::sip::Host::IpAddr(ip) if ip.is_unspecified()) {
        return;
    }
    destination.addr = remote.addr.clone();
}

pub fn check_authorization_headers(
    req: &rsipstack::sip::Request,
) -> Result<Option<(SipUser, Authorization)>> {
    // First try Authorization header (for backward compatibility with existing tests)
    if let Some(auth_header) = rsipstack::sip_header_opt!(req.headers.iter(), Header::Authorization)
    {
        let challenge = Authorization::parse(auth_header.value())?;
        let user = SipUser {
            username: challenge.username.to_string(),
            realm: Some(challenge.realm.to_string()),
            ..Default::default()
        };
        return Ok(Some((user, challenge)));
    }
    // Then try Proxy-Authorization header
    if let Some(proxy_auth_header) =
        rsipstack::sip_header_opt!(req.headers.iter(), Header::ProxyAuthorization)
    {
        let challenge = Authorization::parse(proxy_auth_header.value())?;
        let user = SipUser {
            username: challenge.username.to_string(),
            realm: Some(challenge.realm.to_string()),
            ..Default::default()
        };
        return Ok(Some((user, challenge)));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SipUser::voicemail_disabled ────────────────────────────────────────────

    #[test]
    fn voicemail_disabled_default_is_false() {
        // Voicemail should be active for a user unless explicitly disabled.
        let user = SipUser::default();
        assert!(
            !user.voicemail_disabled,
            "SipUser::default() must have voicemail_disabled = false"
        );
    }

    #[test]
    fn voicemail_disabled_can_be_set_true() {
        let user = SipUser {
            username: "1001".into(),
            voicemail_disabled: true,
            ..Default::default()
        };
        assert!(user.voicemail_disabled);
    }

    #[test]
    fn merge_with_does_not_override_voicemail_disabled_when_true() {
        // If the primary record already has voicemail_disabled = true the
        // merge should **not** overwrite it with the other side's false.
        let mut primary = SipUser {
            username: "1001".into(),
            voicemail_disabled: true,
            ..Default::default()
        };
        let secondary = SipUser {
            username: "1001".into(),
            voicemail_disabled: false,
            email: Some("alice@pbx.local".into()),
            ..Default::default()
        };
        primary.merge_with(&secondary);
        // voicemail_disabled is a plain bool; merge_with only copies optional
        // fields and bool flags using the `if !flag { flag = other }` pattern.
        // voicemail_disabled is intentionally NOT merged (it is not an Option),
        // so the primary side wins.
        assert!(
            primary.voicemail_disabled,
            "primary.voicemail_disabled should remain true after merge"
        );
        // merge_with should still fill in missing optional fields
        assert_eq!(primary.email.as_deref(), Some("alice@pbx.local"));
    }

    #[test]
    fn merge_with_propagates_voicemail_disabled_when_not_yet_set() {
        // A freshly built SipUser (voicemail_disabled = false) that merges with
        // a DB record that also has voicemail_disabled = false stays false.
        let mut primary = SipUser::default();
        let secondary = SipUser {
            voicemail_disabled: false,
            ..Default::default()
        };
        primary.merge_with(&secondary);
        assert!(!primary.voicemail_disabled);
    }

    fn via_derived_destination(via: &str) -> SipAddr {
        let raw = format!(
            "REGISTER sip:pbx.example.com SIP/2.0\r\nVia: {via}\r\nFrom: <sip:u@pbx.example.com>;tag=t1\r\nTo: <sip:u@pbx.example.com>\r\nCall-ID: c1\r\nCSeq: 1 REGISTER\r\nContent-Length: 0\r\n\r\n"
        );
        let request: rsipstack::sip::Request = rsipstack::sip::Request::try_from(raw.as_str())
            .map_err(|e| format!("{e:?}"))
            .unwrap();
        let (transport, target) = rsipstack::transport::SipConnection::parse_target_from_via(
            request.via_header().expect("via"),
        )
        .expect("parse via");
        SipAddr {
            r#type: Some(transport),
            addr: target,
        }
    }

    /// Production fault: a JsSIP WebSocket client's registration must record
    /// the REAL flow address (`112.64.233.138:7318`), never the `.invalid`
    /// flow token from its Via sent-by nor a wildcard listener default —
    /// otherwise in-dialog requests (BYE) dial back to garbage and call-flow
    /// recordings show `dst_addr: 0.0.0.0:5060`.
    #[tokio::test]
    async fn ws_registration_destination_prefers_connection_flow_address() {
        use rsipstack::transport::channel::ChannelConnection;
        use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
        use tokio_util::sync::CancellationToken;

        let (_in_tx, in_rx): (_, UnboundedReceiver<rsipstack::transport::TransportEvent>) =
            unbounded_channel();
        let (out_tx, _out_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        let conn = ChannelConnection::create_connection(
            in_rx,
            out_tx,
            SipAddr {
                r#type: Some(rsipstack::sip::Transport::Udp),
                addr: rsipstack::sip::HostWithPort {
                    host: rsipstack::sip::Host::IpAddr("112.64.233.138".parse().unwrap()),
                    port: Some(7318.into()),
                },
            },
            Some(cancel.child_token()),
        )
        .await
        .expect("channel connection");
        let connection = rsipstack::transport::SipConnection::Channel(conn);
        assert!(connection.is_reliable(), "channel flows are reliable");

        // Via WITHOUT received/rport: sent-by is the raw `.invalid` flow
        // token a browser stack emits (RFC 7118 B.1).
        let mut destination =
            via_derived_destination("SIP/2.0/WSS 7i94k9e6mr86.invalid;branch=z9hG4bK2011401");
        assert!(
            destination.addr.host.to_string().contains(".invalid"),
            "precondition: via-derived target is the flow token"
        );

        apply_flow_destination(&mut destination, Some(&connection));
        assert_eq!(destination.addr.host.to_string(), "112.64.233.138");
        assert_eq!(
            destination.addr.port.map(|p| p.0),
            Some(7318),
            "the concrete flow ip:port must replace the `.invalid` token"
        );
        assert_eq!(
            destination.r#type,
            Some(rsipstack::sip::Transport::Wss),
            "the Via transport tag must be preserved"
        );
    }

    #[test]
    fn udp_registration_destination_keeps_via_derived_target() {
        // UDP has no per-registration flow to reuse: the classic
        // Via received/rport target stays authoritative.
        let mut destination =
            via_derived_destination("SIP/2.0/UDP 58.246.19.74:6988;branch=z9hG4bKx;rport=6988");
        apply_flow_destination(&mut destination, None);
        assert_eq!(destination.addr.host.to_string(), "58.246.19.74");
        assert_eq!(destination.addr.port.map(|p| p.0), Some(6988));
    }

    #[tokio::test]
    async fn wildcard_flow_address_is_ignored() {
        use rsipstack::transport::channel::ChannelConnection;
        use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
        use tokio_util::sync::CancellationToken;

        let (out_tx, _out_rx) = unbounded_channel();
        let (_in_tx, in_rx): (_, UnboundedReceiver<rsipstack::transport::TransportEvent>) =
            unbounded_channel();
        let conn = ChannelConnection::create_connection(
            in_rx,
            out_tx,
            SipAddr {
                r#type: Some(rsipstack::sip::Transport::Udp),
                addr: rsipstack::sip::HostWithPort {
                    host: rsipstack::sip::Host::IpAddr("0.0.0.0".parse().unwrap()),
                    port: Some(5060.into()),
                },
            },
            Some(CancellationToken::new()),
        )
        .await
        .expect("channel connection");
        let connection = rsipstack::transport::SipConnection::Channel(conn);

        let mut destination = via_derived_destination(
            "SIP/2.0/WSS 7i94k9e6mr86.invalid;branch=z9hG4bK2011401;received=112.64.233.138;rport=7318",
        );
        apply_flow_destination(&mut destination, Some(&connection));
        assert_eq!(
            destination.addr.host.to_string(),
            "112.64.233.138",
            "a wildcard flow address must not override a usable via-derived target"
        );
    }
}
