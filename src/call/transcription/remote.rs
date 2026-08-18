//! Remote streaming transcription provider (Deepgram-compatible raw-PCM
//! WebSocket protocol).
//!
//! One WebSocket connection per call side (caller / callee) is opened to the
//! configured ASR endpoint. PCM frames are resampled to 16 kHz mono
//! little-endian i16 and streamed as binary messages; the server returns
//! interim (`is_final == false`) and final hypotheses as JSON, which are
//! converted into [`TranscriptSegment`]s.
//!
//! The wire protocol targeted here is Deepgram's `/v1/listen` streaming API,
//! but any endpoint speaking the same envelope (binary PCM in, `{type:
//! "Results", channel.alternatives[0].transcript, is_final}` out) works.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::{
    SidePcmFrame, TranscriptSegment, TranscriptSide, TranscriptionEvent, TranscriptionProvider,
};

/// Bounded per-side PCM queue: a slow network send applies backpressure by
/// dropping the oldest-unsent frames instead of blocking the media pump.
const PCM_QUEUE_CAPACITY: usize = 256;

/// Target PCM rate for the ASR stream.
const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Send a Deepgram KeepAlive text frame every 8s of idle to keep the
/// connection open through NAT/proxy idle timeouts.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(8);

/// Remote streaming ASR configuration (`[proxy.transcript.remote]`).
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct RemoteTranscriptConfig {
    /// ASR WebSocket base URL. Default: `wss://api.deepgram.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// API key. When unset, falls back to the `DEEPGRAM_API_KEY` env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// ASR model name (provider-specific). Default: `nova-2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Language tag (`zh`, `en`, `multi`, ...). Default: `multi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Emit interim (partial) hypotheses. Default: `true`.
    #[serde(default = "default_true", skip_serializing_if = "Option::is_none")]
    pub interim_results: Option<bool>,
    /// Ask the engine to punctuate the output. Default: `true`.
    #[serde(default = "default_true", skip_serializing_if = "Option::is_none")]
    pub punctuate: Option<bool>,
}

fn default_true() -> Option<bool> {
    Some(true)
}

impl RemoteTranscriptConfig {
    fn effective_url(&self) -> String {
        let base = self
            .url
            .clone()
            .unwrap_or_else(|| "wss://api.deepgram.com".to_string());
        let mut full = format!(
            "{}/v1/listen?encoding=linear16&sample_rate={}&channels=1",
            base.trim_end_matches('/'),
            TARGET_SAMPLE_RATE
        );
        if let Some(model) = &self.model {
            full.push_str(&format!("&model={}", urlencode(model)));
        }
        if let Some(language) = &self.language {
            full.push_str(&format!("&language={}", urlencode(language)));
        }
        if self.interim_results.unwrap_or(true) {
            full.push_str("&interim_results=true");
        }
        if self.punctuate.unwrap_or(true) {
            full.push_str("&punctuate=true");
        }
        full
    }

    fn effective_api_key(&self) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| std::env::var("DEEPGRAM_API_KEY").ok())
    }

    /// True when the provider has enough configuration to run (API key from
    /// config or env, and a URL — defaults apply for the rest).
    pub fn is_runnable(&self) -> bool {
        self.effective_api_key().is_some()
    }
}

/// Minimal percent-encoding for query parameter values.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Shared per-side state for timestamp bookkeeping.
#[derive(Default)]
struct SideCursor {
    /// Cumulative milliseconds of audio submitted for this side.
    total_ms: u64,
    /// Cursor at the last final segment (start offset of the next utterance).
    last_final_ms: u64,
}

struct ProviderInner {
    /// Per-side PCM submission queues. Entries are pre-resampled 16k PCM
    /// byte chunks (one per source frame).
    pcm_tx: HashMap<TranscriptSide, mpsc::Sender<Vec<u8>>>,
    cancel: CancellationToken,
}

/// Streaming transcription provider backed by a remote ASR WebSocket service.
pub struct RemoteStreamingProvider {
    inner: Arc<ProviderInner>,
}

impl RemoteStreamingProvider {
    /// Construct the provider and open one ASR connection per requested side.
    ///
    /// `events` receives every recognized segment (and failures) until
    /// [`stop`] is called.
    pub fn new(
        config: RemoteTranscriptConfig,
        sides: &[TranscriptSide],
        events: mpsc::UnboundedSender<TranscriptionEvent>,
    ) -> Self {
        let cancel = CancellationToken::new();
        let mut pcm_tx = HashMap::new();
        for side in sides {
            let (tx, rx) = mpsc::channel::<Vec<u8>>(PCM_QUEUE_CAPACITY);
            pcm_tx.insert(*side, tx);
            tokio::spawn(side_task(
                *side,
                config.clone(),
                rx,
                events.clone(),
                cancel.child_token(),
            ));
        }
        Self {
            inner: Arc::new(ProviderInner { pcm_tx, cancel }),
        }
    }
}

#[async_trait]
impl TranscriptionProvider for RemoteStreamingProvider {
    fn push_pcm(&self, frame: SidePcmFrame) -> anyhow::Result<()> {
        if self.inner.cancel.is_cancelled() {
            anyhow::bail!("transcription provider stopped");
        }
        let Some(tx) = self.inner.pcm_tx.get(&frame.side) else {
            anyhow::bail!("no ASR stream for side {}", frame.side.as_str());
        };
        let pcm = resample_to_16k(&frame.frame.samples, frame.frame.sample_rate);
        // Bounded try_send: on backpressure drop the frame — live media must
        // never block on network I/O.
        tx.try_send(pcm)
            .map_err(|e| anyhow::anyhow!("pcm queue: {e}"))
    }

    async fn stop(&self) {
        self.inner.cancel.cancel();
        // Side tasks observe the token, send CloseStream, and exit.
    }
}

/// Per-side ASR session: drains the PCM queue, streams to the WS endpoint,
/// and translates results into segments. Owns its own connection so a slow
/// or failed side never stalls the other.
async fn side_task(
    side: TranscriptSide,
    config: RemoteTranscriptConfig,
    mut pcm_rx: mpsc::Receiver<Vec<u8>>,
    events: mpsc::UnboundedSender<TranscriptionEvent>,
    cancel: CancellationToken,
) {
    let url = config.effective_url();
    let api_key = config.effective_api_key();

    let mut request =
        match tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(&url) {
            Ok(r) => r,
            Err(e) => {
                warn!(side = side.as_str(), %url, error = %e, "transcript ASR URL invalid");
                let _ = events.send(TranscriptionEvent::Failed {
                    side: Some(side),
                    error: format!("invalid ASR url: {e}"),
                });
                return;
            }
        };
    if let Some(key) = api_key.as_deref() {
        request.headers_mut().insert(
            "Authorization",
            format!("Token {}", key)
                .parse()
                .expect("valid authorization header"),
        );
    }

    let (ws, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(v) => v,
        Err(e) => {
            warn!(side = side.as_str(), error = %e, "transcript ASR connect failed");
            let _ = events.send(TranscriptionEvent::Failed {
                side: Some(side),
                error: format!("ASR connect failed: {e}"),
            });
            return;
        }
    };
    debug!(side = side.as_str(), "transcript ASR stream connected");

    use futures::{SinkExt, StreamExt};
    let (mut sink, mut stream) = ws.split();
    let cursor = Arc::new(std::sync::Mutex::new(SideCursor::default()));

    // Result-forwarding task: parses WS text messages into segments.
    let result_task = {
        let events = events.clone();
        let cursor = cursor.clone();
        let lang = config.language.clone();
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        debug!(side = side.as_str(), error = %e, "transcript ASR read error");
                        continue;
                    }
                };
                let tokio_tungstenite::tungstenite::Message::Text(text) = msg else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if value.get("type").and_then(|t| t.as_str()) != Some("Results") {
                    continue;
                };
                let Some(text) = value
                    .pointer("/channel/alternatives/0/transcript")
                    .and_then(|t| t.as_str())
                else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                let is_final = value
                    .get("is_final")
                    .and_then(|f| f.as_bool())
                    .unwrap_or(false);
                let (start_ms, end_ms) = {
                    let mut c = cursor.lock().unwrap();
                    let end = c.total_ms;
                    let start = c.last_final_ms;
                    if is_final {
                        c.last_final_ms = end;
                    }
                    (start, end)
                };
                if events
                    .send(TranscriptionEvent::Segment(TranscriptSegment {
                        side,
                        text: text.to_string(),
                        partial: !is_final,
                        start_ms,
                        end_ms,
                        lang: lang.clone(),
                    }))
                    .is_err()
                {
                    break;
                }
            }
        })
    };

    // PCM pump: forward queued frames; keepalive when idle.
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending_close = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                pending_close = true;
                break;
            }
            pcm = pcm_rx.recv() => {
                match pcm {
                    Some(bytes) => {
                        let n_samples = bytes.len() / 2;
                        {
                            let mut c = cursor.lock().unwrap();
                            c.total_ms +=
                                (n_samples as u64) * 1000 / TARGET_SAMPLE_RATE as u64;
                        }
                        if sink
                            .send(tokio_tungstenite::tungstenite::Message::Binary(
                                bytes.into(),
                            ))
                            .await
                            .is_err()
                        {
                            let _ = events.send(TranscriptionEvent::Failed {
                                side: Some(side),
                                error: "ASR send failed".to_string(),
                            });
                            break;
                        }
                    }
                    // All provider handles dropped → stop.
                    None => {
                        pending_close = true;
                        break;
                    }
                }
            }
            _ = keepalive.tick() => {
                let _ = sink
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        r#"{"type":"KeepAlive"}"#.into(),
                    ))
                    .await;
            }
        }
    }

    if pending_close {
        // Ask the engine to flush remaining finals.
        let _ = sink
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"type":"CloseStream"}"#.into(),
            ))
            .await;
        // Give the result task a moment to drain finals, then stop everything.
        tokio::time::timeout(Duration::from_secs(2), result_task)
            .await
            .ok();
    }
    cancel.cancel();
    debug!(side = side.as_str(), "transcript ASR stream closed");
}

/// Resample interleaved mono i16 PCM to 16 kHz with linear interpolation.
/// Pass-through when the source is already at the target rate.
fn resample_to_16k(samples: &[i16], from: u32) -> Vec<u8> {
    if from == TARGET_SAMPLE_RATE {
        return samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    }
    let step = from as f64 / TARGET_SAMPLE_RATE as f64;
    let out_len = ((samples.len() as f64) / step).floor() as usize;
    let mut out = Vec::with_capacity(out_len * 2);
    for i in 0..out_len {
        let pos = i as f64 * step;
        let idx = pos as usize;
        let frac = pos - idx as f64;
        let s0 = samples[idx.min(samples.len().saturating_sub(1))] as i32;
        let s1 = samples[(idx + 1).min(samples.len().saturating_sub(1))] as i32;
        let v = (s0 as f64 + (s1 - s0) as f64 * frac).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_target_rate() {
        let out = resample_to_16k(&[1, -1, 2], 16_000);
        assert_eq!(out, vec![1u8, 0, 0xFF, 0xFF, 2, 0]);
    }

    #[test]
    fn upsamples_8k_to_16k() {
        // 8 samples at 8k → 16 samples at 16k.
        let samples: Vec<i16> = (0..8).map(|i| i as i16 * 100).collect();
        let bytes = resample_to_16k(&samples, 8_000);
        assert_eq!(bytes.len(), 32);
        let first = i16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(first, 0);
        let second = i16::from_le_bytes([bytes[2], bytes[3]]);
        assert_eq!(second, 50);
    }

    #[test]
    fn builds_listen_url_with_params() {
        let cfg = RemoteTranscriptConfig {
            model: Some("nova-2".into()),
            language: Some("multi".into()),
            ..Default::default()
        };
        let url = cfg.effective_url();
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?"));
        assert!(url.contains("sample_rate=16000"));
        assert!(url.contains("model=nova-2"));
        assert!(url.contains("language=multi"));
        assert!(url.contains("interim_results=true"));
    }
}
