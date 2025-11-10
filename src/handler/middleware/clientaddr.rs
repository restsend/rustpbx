use axum::extract::{ConnectInfo, FromRequestParts};
use http::{HeaderMap, StatusCode, Uri, request::Parts};
use std::{
    fmt::{self, Formatter},
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

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
        let is_secure = match uri.scheme_str() {
            Some("wss") | Some("https") => true,
            _ => headers
                .get("x-forwarded-proto")
                .map_or(false, |v| v == "https"),
        };

        let mut remote_addr = connect_info
            .unwrap_or_else(|| SocketAddr::from((IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)));

        for header in [
            "x-client-ip",
            "x-forwarded-for",
            "x-real-ip",
            "cf-connecting-ip",
        ] {
            if let Some(value) = headers.get(header) {
                if let Ok(ip) = value.to_str() {
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
