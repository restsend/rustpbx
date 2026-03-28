use crate::media::endpoint::MediaPeer;
use crate::media::source::{AudioMapping, DtmfMapping, TranscodeSpec};

// ---------------------------------------------------------------------------
// DirectionConfig — per-direction transform config
// ---------------------------------------------------------------------------

/// Transform config for one direction of a bridge.
#[derive(Clone, Default)]
pub struct DirectionConfig {
    pub audio: Option<AudioMapping>,
    pub dtmf: Option<DtmfMapping>,
    pub transcode: Option<TranscodeSpec>,
}

impl DirectionConfig {
    /// Build a DirectionConfig from negotiated leg profiles (source → target).
    pub fn from_profiles(
        source: &crate::media::negotiate::NegotiatedLegProfile,
        target: &crate::media::negotiate::NegotiatedLegProfile,
    ) -> Self {
        let audio = match (&source.audio, &target.audio) {
            (Some(sa), Some(ta)) => Some(AudioMapping {
                source_pt: sa.payload_type,
                target_pt: ta.payload_type,
                source_clock_rate: sa.clock_rate,
                target_clock_rate: ta.clock_rate,
                source_codec: sa.codec,
                target_codec: ta.codec,
            }),
            _ => None,
        };

        let transcode = match (&source.audio, &target.audio) {
            (Some(sa), Some(ta)) if sa.codec != ta.codec => Some(TranscodeSpec {
                source_codec: sa.codec,
                target_codec: ta.codec,
                target_pt: ta.payload_type,
            }),
            _ => None,
        };

        let dtmf = source.dtmf.as_ref().map(|sd| DtmfMapping {
            source_pt: sd.payload_type,
            target_pt: target.dtmf.as_ref().map(|d| d.payload_type),
            source_clock_rate: sd.clock_rate,
            target_clock_rate: target.dtmf.as_ref().map(|d| d.clock_rate),
        });

        Self { audio, dtmf, transcode }
    }
}

// ---------------------------------------------------------------------------
// MediaBridge — bidirectional media bridge between two endpoints
// ---------------------------------------------------------------------------

/// Bidirectional media bridge between two media peers.
///
/// Wires each endpoint's receiver track through an optional recording tap
/// and transform filter, then installs the result as the source on the
/// opposite endpoint's output. Poll-based, no spawned tasks.
pub struct MediaBridge {
    caller: MediaPeer,
    callee: MediaPeer,
    caller_to_callee: DirectionConfig,
    callee_to_caller: DirectionConfig,
}

impl MediaBridge {
    /// Start building a bridge with the two required endpoints.
    pub fn builder(caller: MediaPeer, callee: MediaPeer) -> MediaBridgeBuilder {
        MediaBridgeBuilder {
            caller,
            callee,
            caller_to_callee: DirectionConfig::default(),
            callee_to_caller: DirectionConfig::default(),
        }
    }

    /// Wire both directions: caller.input → callee.output and vice versa.
    pub fn bridge(&self) {
        let c2c = &self.caller_to_callee;
        self.callee.output().install_provider(
            self.caller
                .input()
                .provider_for_output(c2c.audio.clone(), c2c.dtmf.clone(), c2c.transcode.clone()),
        );
        let c2c_rev = &self.callee_to_caller;
        self.caller.output().install_provider(
            self.callee
                .input()
                .provider_for_output(
                    c2c_rev.audio.clone(),
                    c2c_rev.dtmf.clone(),
                    c2c_rev.transcode.clone(),
                ),
        );
    }

    /// Remove both directions (switch to idle).
    pub fn unbridge(&self) {
        self.caller.output().clear_provider();
        self.callee.output().clear_provider();
    }

    /// Access the caller endpoint.
    pub fn caller(&self) -> &MediaPeer {
        &self.caller
    }

    /// Access the callee endpoint.
    pub fn callee(&self) -> &MediaPeer {
        &self.callee
    }
}

pub struct MediaBridgeBuilder {
    caller: MediaPeer,
    callee: MediaPeer,
    caller_to_callee: DirectionConfig,
    callee_to_caller: DirectionConfig,
}

impl MediaBridgeBuilder {
    pub fn caller_to_callee(mut self, config: DirectionConfig) -> Self {
        self.caller_to_callee = config;
        self
    }

    pub fn callee_to_caller(mut self, config: DirectionConfig) -> Self {
        self.callee_to_caller = config;
        self
    }

    pub fn build(self) -> MediaBridge {
        MediaBridge {
            caller: self.caller,
            callee: self.callee,
            caller_to_callee: self.caller_to_callee,
            callee_to_caller: self.callee_to_caller,
        }
    }
}
