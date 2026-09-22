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

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use remote::resample_to_16k;


#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptionPlan {
    /// Provider registry key overriding `[proxy.transcript.remote] provider`
    /// for this call (see [`TranscriptionProviderFactory::name`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Language tag overriding the provider's configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Extra HTTP headers attached to the provider's WebSocket handshake
    /// (e.g. gateway auth). Provider-specific; ignored by providers that
    /// don't use them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Vec<(String, String)>>,
}

/// Identity of the call a provider instance is created for, so third-party
/// factories can correlate recognition output back to the originating call
/// (e.g. webhooks keyed by call id).
#[derive(Debug, Clone)]
pub struct TranscriptionCallInfo {
    /// Call id as used across call events / CDRs (normalized SIP Call-ID).
    pub call_id: String,
    /// Owning SIP session id.
    pub session_id: String,
}

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
    Failed {
        side: Option<TranscriptSide>,
        error: String,
    },
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

/// Factory that builds a [`TranscriptionProvider`] for one call.
///
/// This is the third-party extension point: implement this trait (plus
/// [`TranscriptionProvider`]) and register it with
/// [`register_transcription_provider`] — typically from an addon or from the
/// embedding crate's startup — then select it via
/// `[proxy.transcript.remote] provider = "<name>"` in `config.toml`.
///
/// `params` is the serialized `[proxy.transcript.remote]` table: each factory
/// parses whatever keys it needs and ignores the rest (the Deepgram factory
/// parses [`remote::RemoteTranscriptConfig`], which is lenient about unknown
/// keys). Implementations should do their own pre-flight validation here
/// (credentials, endpoints, ...) and return `Err` with a human-readable
/// message; the error is surfaced to subscribers as a `transcript_error` RWI
/// event.
///
/// `create` is synchronous on purpose: providers spawn their own tasks and
/// must never block session startup.
pub trait TranscriptionProviderFactory: Send + Sync {
    /// Registry key, matched against `[proxy.transcript.remote] provider`.
    fn name(&self) -> &str;

    /// Build one provider for a single call. `call` identifies the call the
    /// provider is attached to; `sides` lists the call legs that actually
    /// carry negotiated media; `events` receives [`TranscriptionEvent`]s
    /// until [`TranscriptionProvider::stop`] is called.
    fn create(
        &self,
        call: &TranscriptionCallInfo,
        sides: &[TranscriptSide],
        events: mpsc::UnboundedSender<TranscriptionEvent>,
        params: &serde_json::Value,
    ) -> Result<Arc<dyn TranscriptionProvider>>;
}

/// Global provider registry: name → factory. Seeded with the built-in
/// Deepgram-compatible factory; third parties can add (or override) entries
/// at startup.
static PROVIDER_FACTORIES: LazyLock<
    RwLock<HashMap<String, Arc<dyn TranscriptionProviderFactory>>>,
> = LazyLock::new(|| {
    let mut map: HashMap<String, Arc<dyn TranscriptionProviderFactory>> = HashMap::new();
    let builtin: Arc<dyn TranscriptionProviderFactory> = Arc::new(remote::DeepgramFactory);
    map.insert(builtin.name().to_string(), builtin);
    RwLock::new(map)
});

/// Register (or replace) a transcription provider factory under its
/// [`TranscriptionProviderFactory::name`]. Call from an addon or the
/// embedding crate before the first transcription starts.
pub fn register_transcription_provider(factory: Arc<dyn TranscriptionProviderFactory>) {
    let name = factory.name().to_string();
    // Factory code never runs under this lock (`create` is called on the
    // resolved Arc outside of it), so poisoning is theoretically impossible —
    // recover the guard anyway so an unrelated panic can never wedge
    // transcription startup.
    PROVIDER_FACTORIES
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name, factory);
}

/// Look up a factory by the configured provider name. `None` means no
/// factory is registered under that name.
pub fn resolve_transcription_provider(name: &str) -> Option<Arc<dyn TranscriptionProviderFactory>> {
    PROVIDER_FACTORIES
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// No-op provider used by factory tests; `create` must not touch the
    /// network, so the mock never spawns tasks.
    struct NoopProvider;

    #[async_trait]
    impl TranscriptionProvider for NoopProvider {
        fn push_pcm(&self, _frame: SidePcmFrame) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) {}
    }

    struct MockFactory {
        name: String,
        fail: bool,
        creations: Arc<AtomicUsize>,
    }

    impl TranscriptionProviderFactory for MockFactory {
        fn name(&self) -> &str {
            &self.name
        }
        fn create(
            &self,
            _call: &TranscriptionCallInfo,
            _sides: &[TranscriptSide],
            _events: mpsc::UnboundedSender<TranscriptionEvent>,
            _params: &serde_json::Value,
        ) -> Result<Arc<dyn TranscriptionProvider>> {
            self.creations.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("mock provider unavailable");
            }
            Ok(Arc::new(NoopProvider))
        }
    }

    #[test]
    fn builtin_deepgram_factory_resolves() {
        let factory = resolve_transcription_provider("deepgram");
        assert!(factory.is_some(), "built-in deepgram factory missing");
        assert_eq!(factory.unwrap().name(), "deepgram");
    }

    #[test]
    fn unknown_provider_name_resolves_to_none() {
        assert!(resolve_transcription_provider("no-such-provider").is_none());
    }

    #[test]
    fn registered_factory_is_resolvable_and_can_be_overridden() {
        let creations = Arc::new(AtomicUsize::new(0));
        register_transcription_provider(Arc::new(MockFactory {
            name: "test-mock".to_string(),
            fail: false,
            creations: creations.clone(),
        }));
        let factory =
            resolve_transcription_provider("test-mock").expect("registered factory missing");
        let (tx, _rx) = mpsc::unbounded_channel();
        let provider = factory
            .create(
                &TranscriptionCallInfo {
                    call_id: "test-call".to_string(),
                    session_id: "test-session".to_string(),
                },
                &[TranscriptSide::Caller],
                tx,
                &serde_json::json!({ "anything": true }),
            )
            .expect("mock create should succeed");
        assert_eq!(creations.load(Ordering::SeqCst), 1);
        assert!(
            !provider
                .push_pcm(SidePcmFrame {
                    side: TranscriptSide::Caller,
                    frame: crate::media::AudioFrame {
                        samples: vec![],
                        sample_rate: 8_000,
                        timestamp: 0,
                    },
                })
                .is_err()
        );

        // Re-registering under the same name replaces the previous factory.
        register_transcription_provider(Arc::new(MockFactory {
            name: "test-mock".to_string(),
            fail: true,
            creations,
        }));
        let factory =
            resolve_transcription_provider("test-mock").expect("overridden factory missing");
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = match factory.create(
            &TranscriptionCallInfo {
                call_id: "test-call".to_string(),
                session_id: "test-session".to_string(),
            },
            &[],
            tx,
            &serde_json::json!({}),
        ) {
            Err(e) => e,
            Ok(_) => panic!("overridden factory should fail"),
        };
        assert!(err.to_string().contains("mock provider unavailable"));
    }

    #[test]
    fn deepgram_factory_requires_api_key() {
        let factory = resolve_transcription_provider("deepgram").unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        // Only assert the failure path when DEEPGRAM_API_KEY is not set in
        // the environment (tests may run on machines that export it).
        if std::env::var("DEEPGRAM_API_KEY").is_err() {
            let err = match factory.create(
                &TranscriptionCallInfo {
                    call_id: "test-call".to_string(),
                    session_id: "test-session".to_string(),
                },
                &[],
                tx.clone(),
                &serde_json::json!({}),
            ) {
                Err(e) => e,
                Ok(_) => panic!("missing api_key must be rejected"),
            };
            assert!(err.to_string().contains("api_key"));
        }
        // Empty `sides` means no ASR connection is spawned, so this stays
        // hermetic; with a key present the provider constructs fine.
        let provider = factory
            .create(
                &TranscriptionCallInfo {
                    call_id: "test-call".to_string(),
                    session_id: "test-session".to_string(),
                },
                &[],
                tx,
                &serde_json::json!({ "api_key": "test-key" }),
            )
            .expect("api_key present should construct");
        drop(provider);
    }
}
