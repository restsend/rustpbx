use super::{BodyFormat, HttpTtsConfig};
use anyhow::{anyhow, Result};
use std::time::Duration;

pub async fn synthesize_http(
    cfg: &HttpTtsConfig,
    client: &reqwest::Client,
    text: &str,
    voice: Option<&str>,
) -> Result<bytes::Bytes> {
    let mut params = cfg.extra_params.clone();
    params.insert(cfg.param_name.clone(), text.to_string());
    if let Some(v) = voice {
        params.insert("voice".to_string(), v.to_string());
    }

    let mut req = match cfg.body_format {
        BodyFormat::Query | BodyFormat::Form => {
            if cfg.method.eq_ignore_ascii_case("GET") {
                client.get(&cfg.url).query(&params)
            } else if cfg.method.eq_ignore_ascii_case("POST") {
                if matches!(cfg.body_format, BodyFormat::Query) {
                    client.post(&cfg.url).query(&params)
                } else {
                    client.post(&cfg.url).form(&params)
                }
            } else {
                client
                    .request(
                        reqwest::Method::from_bytes(cfg.method.as_bytes())?,
                        &cfg.url,
                    )
                    .query(&params)
            }
        }
        BodyFormat::Json => {
            let body = serde_json::to_value(&params)?;
            client
                .request(
                    reqwest::Method::from_bytes(cfg.method.as_bytes())?,
                    &cfg.url,
                )
                .json(&body)
        }
    };

    for (key, value) in &cfg.headers {
        req = req.header(key, value);
    }

    let resp = tokio::time::timeout(
        Duration::from_secs(cfg.timeout_seconds),
        req.send(),
    )
    .await
    .map_err(|_| anyhow!("TTS HTTP request timed out after {}s", cfg.timeout_seconds))?
    .map_err(|e| anyhow!("TTS HTTP request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(anyhow!("TTS HTTP returned {}", resp.status()));
    }

    resp.bytes()
        .await
        .map_err(|e| anyhow!("Failed to read TTS response body: {}", e))
}
