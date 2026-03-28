use crate::media::endpoint::{PeerInput, PeerOutput};
use crate::media::source::{AudioMapping, DtmfMapping, TranscodeSpec};
use std::sync::Arc;

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
// MediaBridge — bidirectional media bridge between two peers
// ---------------------------------------------------------------------------

/// Bidirectional media bridge wiring two peers' inputs and outputs.
///
/// Connects caller.input → callee.output and callee.input → caller.output
/// with per-direction transform config. Poll-based, no spawned tasks.
pub struct MediaBridge {
    caller_input: PeerInput,
    caller_output: Arc<PeerOutput>,
    callee_input: PeerInput,
    callee_output: Arc<PeerOutput>,
    caller_to_callee: DirectionConfig,
    callee_to_caller: DirectionConfig,
}

impl MediaBridge {
    /// Start building a bridge.
    pub fn builder(
        caller_input: PeerInput,
        caller_output: Arc<PeerOutput>,
        callee_input: PeerInput,
        callee_output: Arc<PeerOutput>,
    ) -> MediaBridgeBuilder {
        MediaBridgeBuilder {
            caller_input,
            caller_output,
            callee_input,
            callee_output,
            caller_to_callee: DirectionConfig::default(),
            callee_to_caller: DirectionConfig::default(),
        }
    }

    /// Wire both directions: caller.input → callee.output and vice versa.
    pub fn bridge(&self) {
        let c2c = &self.caller_to_callee;
        self.callee_output.set_input(
            self.caller_input
                .adapted_for_output(c2c.audio.clone(), c2c.dtmf.clone(), c2c.transcode.clone()),
        );
        let c2c_rev = &self.callee_to_caller;
        self.caller_output.set_input(
            self.callee_input
                .adapted_for_output(
                    c2c_rev.audio.clone(),
                    c2c_rev.dtmf.clone(),
                    c2c_rev.transcode.clone(),
                ),
        );
    }

    /// Remove both directions (switch to idle).
    pub fn unbridge(&self) {
        self.caller_output.clear_input();
        self.callee_output.clear_input();
    }

    pub fn caller_output(&self) -> &Arc<PeerOutput> {
        &self.caller_output
    }

    pub fn callee_output(&self) -> &Arc<PeerOutput> {
        &self.callee_output
    }
}

pub struct MediaBridgeBuilder {
    caller_input: PeerInput,
    caller_output: Arc<PeerOutput>,
    callee_input: PeerInput,
    callee_output: Arc<PeerOutput>,
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
            caller_input: self.caller_input,
            caller_output: self.caller_output,
            callee_input: self.callee_input,
            callee_output: self.callee_output,
            caller_to_callee: self.caller_to_callee,
            callee_to_caller: self.callee_to_caller,
        }
    }
}
