//! Live call transcription: PCM-in / text-out provider abstraction.
//!
//! A [`TranscriptionProvider`] receives decoded PCM frames per call side
//! (caller / callee) while a call is active and emits [`TranscriptSegment`]s
//! (partial + final) through an internal channel. The session-level
//! orchestration lives in
//! `src/proxy/proxy_call/sip_session/live_transcription.rs`; concrete
//! providers live here.
//!
//! The first (and default) implementation is [`remote::RemoteStreamingProvider`]
//! which streams PCM to a cloud ASR endpoint (Deepgram-compatible raw-PCM
//! WebSocket protocol) and returns interim / final hypotheses.

pub mod remote;

use async_trait::async_trait;
use serde::Serialize;

/// Which call participant produced the audio for a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptSide {
    Caller,
    Callee,
}

impl TranscriptSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            TranscriptSide::Caller => "caller",
            TranscriptSide::Callee => "callee",
        }
    }

    pub fn from_leg_side(side: crate::media::media_bridge::LegSide) -> Self {
        match side {
            crate::media::media_bridge::LegSide::A => TranscriptSide::Caller,
            crate::media::media_bridge::LegSide::B => TranscriptSide::Callee,
        }
    }
}

/// One transcribed utterance (or partial hypothesis) from one call side.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptSegment {
    pub side: TranscriptSide,
    /// Recognized text. May be refined across consecutive partials of the
    /// same utterance; the final segment (`partial == false`) is definitive.
    pub text: String,
    /// `true` for interim hypotheses, `false` for the finalized utterance.
    pub partial: bool,
    /// Offset from transcription start, in milliseconds.
    pub start_ms: u64,
    /// Offset from transcription start, in milliseconds.
    pub end_ms: u64,
    /// Detected / configured language, when known.
    pub lang: Option<String>,
}

/// Everything a provider reports upwards while running.
#[derive(Debug)]
pub enum TranscriptionEvent {
    /// A recognized (partial or final) segment.
    Segment(TranscriptSegment),
    /// The provider could not start or has died. `side` is `None` when the
    /// whole provider is affected.
    Failed { side: Option<TranscriptSide>, error: String },
}

/// A PCM frame tagged with the call side it came from.
#[derive(Debug)]
pub struct SidePcmFrame {
    pub side: TranscriptSide,
    pub frame: crate::media::AudioFrame,
}

/// PCM-in / text-out transcription provider.
///
/// Implementations must be cheap to clone-handle from the caller's side: the
/// session pump calls [`TranscriptionProvider::push_pcm`] on every 20ms frame
/// (non-blocking, bounded internal buffering), while network I/O runs on the
/// provider's own tasks. Segments are delivered through the sender supplied at
/// construction time.
#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    /// Non-blocking frame submission. A `Err` return means the provider's
    /// internal queue is full or the provider has stopped; the pump should
    /// drop the frame (never block the media path).
    fn push_pcm(&self, frame: SidePcmFrame) -> anyhow::Result<()>;

    /// Signal end of audio (e.g. on keepalive flush); implementations may
    /// use it to request a final hypothesis from the engine. Non-blocking.
    fn flush(&self) {}

    /// Stop the provider: closes engine connections and finalizes. Idempotent.
    /// Subsequent `push_pcm` calls are no-ops.
    async fn stop(&self);
}
