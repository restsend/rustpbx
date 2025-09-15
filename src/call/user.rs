use anyhow::Result;
use rsip::{
    Header,
    headers::auth::Algorithm,
    prelude::{HeadersExt, ToTypedHeader},
    typed::Authorization,
};
use rsipstack::{
    transaction::transaction::Transaction,
    transport::{SipAddr, SipConnection},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SipUser {
    #[serde(default)]
    pub id: u64,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub username: String,
    pub password: Option<String>,
    pub realm: Option<String>,
    pub department_id: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub note: Option<String>,
    #[serde(skip)]
    pub origin_contact: Option<rsip::typed::Contact>,
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
            destination: None,
            is_support_webrtc: false,
            department_id: None,
            display_name: None,
            email: None,
            phone: None,
            note: None,
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

    pub fn build_contact_from_invite(&self, tx: &Transaction) -> Option<rsip::typed::Contact> {
        let addr = match tx.endpoint_inner.get_addrs().first() {
            Some(addr) => addr.clone(),
            None => return None,
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
        Some(contact)
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
        let (username, realm) = match check_authorization_headers(&tx.original) {
            Ok(Some((user, _))) => (user.username, user.realm),
            _ => {
                let username = tx
                    .original
                    .from_header()?
                    .uri()?
                    .user()
                    .unwrap_or_default()
                    .to_string();
                let realm = tx.original.uri().host().to_string();
                (username, Some(realm))
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
        let is_support_webrtc = destination
            .r#type
            .is_some_and(|t| t == rsip::transport::Transport::Wss);

        Ok(SipUser {
            id: 0,
            username,
            password: None,
            enabled: true,
            realm,
            origin_contact,
            destination: Some(destination),
            is_support_webrtc,
            department_id: None,
            display_name: None,
            email: None,
            phone: None,
            note: None,
        })
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
        let challenge = proxy_auth_header.typed()?;
        let user = SipUser {
            username: challenge.0.username.to_string(),
            realm: Some(challenge.0.realm.to_string()),
            ..Default::default()
        };
        return Ok(Some((user, challenge.0)));
    }

    Ok(None)
}
