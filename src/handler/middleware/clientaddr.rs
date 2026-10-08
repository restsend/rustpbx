use axum::extract::{ConnectInfo, FromRequestParts};
use http::{HeaderMap, StatusCode, Uri, request::Parts};
use std::{
    fmt::{self, Formatter},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::atomic::{AtomicBool, Ordering},
};

/// Global switch: may `X-Forwarded-*` / `X-Real-IP` / `X-Client-IP` /
/// `CF-Connecting-IP` headers be trusted when deriving the client address and
/// request scheme?
///
/// Defaults to **false**. These headers are trivially spoofable by any direct
/// client, and the spoofed value feeds security decisions (AMI IP allowlist,
/// `Secure` cookie attribute, ban lists). Set `trust_forward_headers = true`
/// in the config only when RustPBX runs behind a reverse proxy that
/// unconditionally overwrites them. Wired from `Config::trust_forward_headers`
/// at startup in `src/builder/mod.rs`.
static TRUST_FORWARDED_HEADERS: AtomicBool = AtomicBool::new(false);

pub fn set_trust_forwarded_headers(trusted: bool) {
    TRUST_FORWARDED_HEADERS.store(trusted, Ordering::Relaxed);
}

pub fn forwarded_headers_trusted() -> bool {
    TRUST_FORWARDED_HEADERS.load(Ordering::Relaxed)
}

pub struct ClientAddr {
    pub addr: SocketAddr,
    pub is_secure: bool,
}

impl ClientAddr {
    pub fn new(addr: SocketAddr) -> Self {
        ClientAddr {
            addr,
            is_secure: false,
        }
    }
    pub fn ip(&self) -> IpAddr {
        self.addr.ip()
    }

    pub fn from_http_parts(
        uri: &Uri,
        headers: &HeaderMap,
        connect_info: Option<SocketAddr>,
    ) -> Self {
        let trust_forwarded = forwarded_headers_trusted();
        let is_secure = match uri.scheme_str() {
            Some("wss") | Some("https") => true,
            _ => {
                trust_forwarded
                    && headers
                        .get("x-forwarded-proto")
                        .is_some_and(|v| v == "https")
            }
        };

        let mut remote_addr = connect_info
            .unwrap_or_else(|| SocketAddr::from((IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)));

        // Forwarding headers are only honored when the operator explicitly
        // declared a trusted proxy (see `trust_forward_headers`). Without that
        // opt-in they are ignored: otherwise `X-Forwarded-For: 127.0.0.1` would
        // let any remote caller pass IP allowlists (AMI, bans, rate limits).
        if trust_forwarded {
            for header in [
                "x-client-ip",
                "x-forwarded-for",
                "x-real-ip",
                "cf-connecting-ip",
            ] {
                if let Some(value) = headers.get(header)
                    && let Ok(ip) = value.to_str()
                {
                    let first_ip = ip.split(',').next().unwrap_or(ip).trim();
                    if let Ok(parsed_ip) = first_ip.parse::<IpAddr>() {
                        remote_addr.set_ip(parsed_ip);
                    }
                    break;
                }
            }
        }

        ClientAddr {
            addr: remote_addr,
            is_secure,
        }
    }
}

impl<S> FromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let connect_info = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| *addr);

        Ok(ClientAddr::from_http_parts(
            &parts.uri,
            &parts.headers,
            connect_info,
        ))
    }
}

impl fmt::Display for ClientAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.addr)
    }
}
