use std::net::IpAddr;

pub const DEFAULT_AUTO_EXTERNAL_IP_URL: &str = "http://ifconfig.me";

pub async fn detect_external_ip(url: &str) -> Result<IpAddr, anyhow::Error> {
    let url = if url.is_empty() {
        DEFAULT_AUTO_EXTERNAL_IP_URL
    } else {
        url
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("curl/8.7.1")
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build http client: {}", e))?;

    let body = client
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("failed to fetch {}: {}", url, e))?
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read response body from {}: {}", url, e))?;

    let ip_str = body.trim();
    let ip: IpAddr = ip_str
        .parse()
        .map_err(|_| anyhow::anyhow!("response from {} is not a valid IP address: {}", url, ip_str))?;

    Ok(ip)
}

