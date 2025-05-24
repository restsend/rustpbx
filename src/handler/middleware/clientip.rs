use axum::extract::{ConnectInfo, FromRequestParts};
use http::{request::Parts, StatusCode};
use std::{
    fmt::{self, Formatter},
    net::SocketAddr,
};

pub struct ClientIp(String);

impl ClientIp {
    pub fn new(ip: String) -> Self {
        ClientIp(ip)
    }
}

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Try to get IP from common proxy headers
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
                    return Ok(ClientIp(first_ip.to_string()));
                }
            }
        }

        // Fall back to connection info if available
        if let Some(ConnectInfo(addr)) = parts.extensions.get::<ConnectInfo<SocketAddr>>() {
            return Ok(ClientIp(addr.ip().to_string()));
        }

        // Default fallback
        Ok(ClientIp("*:*".to_string()))
    }
}

impl fmt::Display for ClientIp {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
