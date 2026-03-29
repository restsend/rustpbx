use crate::media::endpoint::{PeerInput, PeerOutput};
pub use crate::media::source::BridgeInputConfig as DirectionConfig;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// DirectionConfig — per-direction transform config
// ---------------------------------------------------------------------------
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
        let c2c = self.caller_to_callee.clone();
        self.callee_output.set_input(
            self.caller_input.adapted_for_output(c2c),
        );
        let c2c_rev = self.callee_to_caller.clone();
        self.caller_output.set_input(
            self.callee_input.adapted_for_output(c2c_rev),
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
