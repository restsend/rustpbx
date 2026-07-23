use std::net::IpAddr;

use crate::http_util::{HttpFetchOptions, fetch_text};

pub const DEFAULT_AUTO_EXTERNAL_IP_URL: &str = "http://ifconfig.me";

pub async fn detect_external_ip(url: &str) -> Result<IpAddr, anyhow::Error> {
    let url = if url.is_empty() {
        DEFAULT_AUTO_EXTERNAL_IP_URL
    } else {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(anyhow::anyhow!(
                "External IP URL must start with http:// or https://"
            ));
        }
        url
    };

    // Prevent SSRF — only allow public external IP detection services
    if !crate::utils::is_url_ssrf_safe(url) {
        return Err(anyhow::anyhow!(
            "External IP URL points to a private or internal address"
        ));
    }

    let opts = HttpFetchOptions::new()
        .with_timeout(std::time::Duration::from_secs(10))
        .with_header("User-Agent", "curl/8.7.1");

    let body = fetch_text(crate::http_util::shared_keepalive_client(), url, &opts).await?;
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
