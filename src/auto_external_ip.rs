use std::net::IpAddr;

use crate::http_util::{HttpFetchOptions, fetch_text};

pub const DEFAULT_AUTO_EXTERNAL_IP_URL: &str = "http://ifconfig.me";

pub async fn detect_external_ip(url: &str) -> Result<IpAddr, anyhow::Error> {
    let url = if url.is_empty() {
        DEFAULT_AUTO_EXTERNAL_IP_URL
    } else {
        url
    };

    let opts = HttpFetchOptions::new()
        .with_timeout(std::time::Duration::from_secs(10))
        .with_header("User-Agent", "curl/8.7.1");

    let body = fetch_text(&reqwest::Client::new(), url, &opts).await?;
    let ip_str = body.trim();
    let ip: IpAddr = ip_str.parse().map_err(|_| {
        anyhow::anyhow!(
            "response from {} is not a valid IP address: {}",
            url,
            ip_str
        )
    })?;

    Ok(ip)
}
