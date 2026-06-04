use super::{BodyFormat, HttpTtsConfig};
use crate::http_util::execute_request;
use anyhow::{Result, anyhow};
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

    let req = match cfg.body_format {
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

    let resp = execute_request(
        req,
        &cfg.headers,
        Some(Duration::from_secs(cfg.timeout_seconds)),
    )
    .await?;

    resp.bytes()
        .await
        .map_err(|e| anyhow!("Failed to read TTS response body: {}", e))
}
