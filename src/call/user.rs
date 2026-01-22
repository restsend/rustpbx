use anyhow::Result;
use rsip::{
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
    #[serde(default)]
    pub call_forwarding_mode: Option<String>,
    #[serde(default)]
    pub call_forwarding_destination: Option<String>,
    #[serde(default)]
    pub call_forwarding_timeout: Option<i32>,
    /// From the original INVITE
    #[serde(skip)]
    pub origin_contact: Option<rsip::typed::Contact>,
    /// Current contact (may be updated by REGISTER)
    #[serde(skip)]
    pub contact: Option<rsip::typed::Contact>,
    #[serde(skip)]
    pub from: Option<rsip::Uri>,
    #[serde(skip)]
    pub destination: Option<SipAddr>,
    #[serde(default = "default_is_support_webrtc")]
    pub is_support_webrtc: bool,
}

impl ToString for SipUser {
    fn to_string(&self) -> String {
        if let Some(realm) = &self.realm {
            format!("{}@{}", self.username, realm)
        } else {
            self.username.clone()
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
            Some(rsip::Transport::Udp) | None => {}
            Some(t) => {
                contact_params.push(rsip::Param::Transport(t));
            }
        }
        let contact = rsip::typed::Contact {
            display_name: None,
            uri: rsip::Uri {
                scheme: addr.r#type.map(|t| t.sip_scheme()),
                auth: Some(rsip::Auth {
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
        match algorithm {
            Algorithm::Md5 | Algorithm::Md5Sess => {
                let mut hasher = Md5::new();
                hasher.update(value);
                format!("{:x}", hasher.finalize())
            }
            Algorithm::Sha256 | Algorithm::Sha256Sess => {
                let mut hasher = Sha256::new();
                hasher.update(value);
                format!("{:x}", hasher.finalize())
            }
            Algorithm::Sha512 | Algorithm::Sha512Sess => {
                let mut hasher = Sha512::new();
                hasher.update(value);
                format!("{:x}", hasher.finalize())
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
                let realm = if let Some(port) = from_uri.port() {
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

        let destination = SipAddr {
            r#type: Some(via_transport),
            addr: destination_addr,
        };

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
        };
        u.build_contact(tx);
        Ok(u)
    }
}

pub fn check_authorization_headers(
    req: &rsip::Request,
) -> Result<Option<(SipUser, Authorization)>> {
    // First try Authorization header (for backward compatibility with existing tests)
    if let Some(auth_header) = rsip::header_opt!(req.headers.iter(), Header::Authorization) {
        let challenge = auth_header.typed()?;
        let user = SipUser {
            username: challenge.username.to_string(),
            realm: Some(challenge.realm.to_string()),
            ..Default::default()
        };
        return Ok(Some((user, challenge)));
    }
    // Then try Proxy-Authorization header
    if let Some(proxy_auth_header) =
        rsip::header_opt!(req.headers.iter(), Header::ProxyAuthorization)
    {
        let challenge = proxy_auth_header.typed()?.0;
        let user = SipUser {
            username: challenge.username.to_string(),
            realm: Some(challenge.realm.to_string()),
            ..Default::default()
        };
        return Ok(Some((user, challenge)));
    }

    Ok(None)
}
