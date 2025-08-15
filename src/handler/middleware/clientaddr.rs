use axum::extract::{ConnectInfo, FromRequestParts};
use http::{StatusCode, request::Parts};
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
}

impl<S> FromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let is_secure = match parts.uri.scheme_str() {
            Some("wss") | Some("https") => true,
            _ => parts
                .headers
                .get("x-forwarded-proto")
                .map_or(false, |v| v == "https"),
        };
        let mut remote_addr = match parts.extensions.get::<ConnectInfo<SocketAddr>>() {
            Some(ConnectInfo(addr)) => addr.clone(),
            None => {
                return Ok(ClientAddr {
                    addr: SocketAddr::from((IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)),
                    is_secure,
                });
            }
        };

        for header in [
            "x-client-ip",
            "x-forwarded-for",
            "x-real-ip",
            "cf-connecting-ip",
        ] {
            if let Some(value) = parts.headers.get(header) {
                if let Ok(ip) = value.to_str() {
                    // Handle comma-separated IPs (e.g. X-Forwarded-For can have multiple)
                    let first_ip = ip.split(',').next().unwrap_or(ip).trim();
                    remote_addr.set_ip(IpAddr::V4(first_ip.parse().unwrap()));
                    break;
                }
            }
        }
        Ok(ClientAddr {
            addr: remote_addr,
            is_secure,
        })
    }
}

impl fmt::Display for ClientAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.addr)
    }
}
