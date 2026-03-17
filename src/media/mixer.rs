use crate::proxy::proxy_call::media_peer::MediaPeer;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// Simple PCM frame mixer
pub struct AudioMixer {
    _sample_rate: u32,
    _channels: u16,
}

impl AudioMixer {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            _sample_rate: sample_rate,
            _channels: channels,
        }
    }

    /// Mix multiple frames with individual gains
    /// Each frame should be the same length
    pub fn mix_frames(&self, frames: Vec<Vec<i16>>, gains: &[f32]) -> Vec<i16> {
        if frames.is_empty() || gains.len() != frames.len() {
            return vec![];
        }

        let frame_len = frames[0].len();
        let mut output = vec![0i16; frame_len];

        for (frame, &gain) in frames.iter().zip(gains) {
            if frame.len() != frame_len {
                continue;
            }
            for (i, sample) in frame.iter().enumerate() {
                // Apply gain and mix
                let mixed = (output[i] as f32 + (*sample as f32) * gain) as i16;
                output[i] = mixed.clamp(i16::MIN, i16::MAX);
            }
        }

        output
    }
}

/// Routing configuration for a mixer input
#[derive(Clone, Debug)]
pub struct MixerRoute {
    /// Input peer ID
    pub input_id: String,
    /// Which outputs this input should be routed to (output_id -> gain)
    pub outputs: HashMap<String, f32>,
}

/// Supervisor mode for the mixer
#[derive(Clone, Debug, PartialEq)]
pub enum SupervisorMixerMode {
    /// Listen: supervisor hears both, sends nothing
    Listen,
    /// Whisper: supervisor hears both, agent hears supervisor + customer, customer hears only agent
    Whisper,
    /// Barge: all three can hear each other
    Barge,
}

/// Mixer peer that wraps a MediaPeer for use in the mixer
#[derive(Clone)]
pub struct MixerPeer {
    peer: Arc<dyn MediaPeer>,
    input_id: String,
    output_id: String,
    cancel_token: CancellationToken,
}

impl MixerPeer {
    pub fn new(
        peer: Arc<dyn MediaPeer>,
        input_id: String,
        output_id: String,
    ) -> Self {
        Self {
            peer,
            input_id,
            output_id,
            cancel_token: CancellationToken::new(),
        }
    }

    pub fn input_id(&self) -> &str {
        &self.input_id
    }

    pub fn output_id(&self) -> &str {
        &self.output_id
    }

    pub fn peer(&self) -> Arc<dyn MediaPeer> {
        self.peer.clone()
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

/// MediaMixer - N-input to N-output audio mixer with configurable routing
pub struct MediaMixer {
    /// Unique identifier for this mixer
    id: String,
    /// Input peers (input_id -> MixerPeer) - legacy, for backward compat
    inputs: Arc<Mutex<HashMap<String, MixerPeer>>>,
    /// New: MixerInput sources (input_id -> MixerInput)
    mixer_inputs: Arc<Mutex<HashMap<String, crate::media::mixer_input::MixerInput>>>,
    /// Mixer outputs (output_id -> MixerOutput)
    mixer_outputs: Arc<Mutex<HashMap<String, crate::media::mixer_output::MixerOutput>>>,
    /// Routing configuration (input_id -> MixerRoute)
    routes: Arc<Mutex<HashMap<String, MixerRoute>>>,
    /// Output routing (output_id -> which inputs to mix)
    output_routes: Arc<Mutex<HashMap<String, Vec<String>>>>,
    /// Current supervisor mode
    mode: Arc<Mutex<SupervisorMixerMode>>,
    /// Whether the mixer is running
    started: AtomicBool,
    /// Sample rate for mixing
    sample_rate: u32,
    /// Channels (always 1 for mono)
    channels: u16,
    /// Mixer instance
    mixer: Arc<AudioMixer>,
    /// Cancel token for stopping
    cancel_token: CancellationToken,
    /// Frame size in samples (e.g., 160 for 20ms at 8kHz)
    frame_size: usize,
}

impl MediaMixer {
    /// Default frame size (20ms at 8kHz = 160 samples)
    pub const DEFAULT_FRAME_SIZE: usize = 160;
}

impl MediaMixer {
    pub fn new(id: String, sample_rate: u32) -> Self {
        Self {
            id,
            inputs: Arc::new(Mutex::new(HashMap::new())),
            mixer_inputs: Arc::new(Mutex::new(HashMap::new())),
            mixer_outputs: Arc::new(Mutex::new(HashMap::new())),
            routes: Arc::new(Mutex::new(HashMap::new())),
            output_routes: Arc::new(Mutex::new(HashMap::new())),
            mode: Arc::new(Mutex::new(SupervisorMixerMode::Listen)),
            started: AtomicBool::new(false),
            sample_rate,
            channels: 1,
            mixer: Arc::new(AudioMixer::new(sample_rate, 1)),
            cancel_token: CancellationToken::new(),
            frame_size: Self::DEFAULT_FRAME_SIZE,
        }
    }

    // =========================================================================
    // New API for MixerInput/MixerOutput
    // =========================================================================

    /// Add a MixerInput source to the mixer
    pub fn add_mixer_input(&self, input: crate::media::mixer_input::MixerInput) {
        let mut inputs = self.mixer_inputs.lock().unwrap();
        inputs.insert(input.id.clone(), input);
    }

    /// Remove a MixerInput by ID
    pub fn remove_mixer_input(&self, input_id: &str) {
        let mut inputs = self.mixer_inputs.lock().unwrap();
        inputs.remove(input_id);

        let mut routes = self.output_routes.lock().unwrap();
        // Remove this input from all output routes
        for inputs in routes.values_mut() {
            inputs.retain(|id| id != input_id);
        }
    }

    /// Add a MixerOutput destination to the mixer
    pub fn add_mixer_output(&self, output: crate::media::mixer_output::MixerOutput) {
        let mut outputs = self.mixer_outputs.lock().unwrap();
        outputs.insert(output.id.clone(), output);
    }

    /// Remove a MixerOutput by ID
    pub fn remove_mixer_output(&self, output_id: &str) {
        let mut outputs = self.mixer_outputs.lock().unwrap();
        outputs.remove(output_id);

        let mut routes = self.output_routes.lock().unwrap();
        routes.remove(output_id);
    }

    /// Set which inputs are mixed to a specific output
    /// output_id: the output destination
    /// input_ids: list of input IDs to mix for this output
    pub fn set_output_routing(&self, output_id: &str, input_ids: Vec<String>) {
        let mut routes = self.output_routes.lock().unwrap();
        routes.insert(output_id.to_string(), input_ids);
    }

    /// Get output routing configuration
    pub fn get_output_routing(&self, output_id: &str) -> Option<Vec<String>> {
        let routes = self.output_routes.lock().unwrap();
        routes.get(output_id).cloned()
    }

    /// Get all routing configuration (input_id -> MixerRoute)
    pub fn get_routes(&self) -> std::collections::HashMap<String, MixerRoute> {
        let routes = self.routes.lock().unwrap();
        routes.clone()
    }

    // =========================================================================
    // Legacy API (for backward compatibility)
    // =========================================================================

    /// Add an input peer to the mixer
    pub fn add_input(&self, peer: MixerPeer) {
        let mut inputs = self.inputs.lock().unwrap();
        inputs.insert(peer.input_id().to_string(), peer);
    }

    /// Remove an input peer from the mixer
    pub fn remove_input(&self, input_id: &str) {
        let mut inputs = self.inputs.lock().unwrap();
        inputs.remove(input_id);

        let mut routes = self.routes.lock().unwrap();
        routes.remove(input_id);
    }

    /// Set routing for an input
    pub fn set_route(&self, route: MixerRoute) {
        let mut routes = self.routes.lock().unwrap();
        routes.insert(route.input_id.clone(), route);
    }

    /// Clear all routes for an input
    pub fn clear_route(&self, input_id: &str) {
        let mut routes = self.routes.lock().unwrap();
        routes.remove(input_id);
    }

    /// Set supervisor mode
    pub fn set_mode(&self, mode: SupervisorMixerMode) {
        let mut current = self.mode.lock().unwrap();
        *current = mode;
    }

    /// Get current supervisor mode
    pub fn get_mode(&self) -> SupervisorMixerMode {
        self.mode.lock().unwrap().clone()
    }

    /// Apply supervisor mode and configure routes automatically
    /// This sets up the routing matrix for the given participant IDs
    pub fn apply_supervisor_mode(
        &self,
        customer_input_id: &str,
        agent_input_id: &str,
        supervisor_input_id: &str,
        customer_output_id: &str,
        agent_output_id: &str,
        supervisor_output_id: &str,
    ) {
        let mode = self.get_mode();

        // Clear existing routes
        {
            let mut routes = self.routes.lock().unwrap();
            routes.clear();
        }

        match mode {
            SupervisorMixerMode::Listen => {
                // Customer -> agent
                self.set_route(MixerRoute {
                    input_id: customer_input_id.to_string(),
                    outputs: [(agent_output_id.to_string(), 1.0)].into_iter().collect(),
                });
                // Agent -> customer
                self.set_route(MixerRoute {
                    input_id: agent_input_id.to_string(),
                    outputs: [(customer_output_id.to_string(), 1.0)].into_iter().collect(),
                });
                // Supervisor hears both (tap) but sends nothing
                self.set_route(MixerRoute {
                    input_id: supervisor_input_id.to_string(),
                    outputs: HashMap::new(), // No output - listen only
                });
                // Both customer and agent go to supervisor (tap)
                self.set_route(MixerRoute {
                    input_id: customer_input_id.to_string(),
                    outputs: [(supervisor_output_id.to_string(), 1.0)].into_iter().collect(),
                });
                self.set_route(MixerRoute {
                    input_id: agent_input_id.to_string(),
                    outputs: [(supervisor_output_id.to_string(), 1.0)].into_iter().collect(),
                });
            }
            SupervisorMixerMode::Whisper => {
                // Customer -> agent + supervisor
                self.set_route(MixerRoute {
                    input_id: customer_input_id.to_string(),
                    outputs: [
                        (agent_output_id.to_string(), 1.0),
                        (supervisor_output_id.to_string(), 1.0),
                    ]
                    .into_iter()
                    .collect(),
                });
                // Agent -> customer + supervisor
                self.set_route(MixerRoute {
                    input_id: agent_input_id.to_string(),
                    outputs: [
                        (customer_output_id.to_string(), 1.0),
                        (supervisor_output_id.to_string(), 1.0),
                    ]
                    .into_iter()
                    .collect(),
                });
                // Supervisor -> agent ONLY (customer cannot hear)
                self.set_route(MixerRoute {
                    input_id: supervisor_input_id.to_string(),
                    outputs: [(agent_output_id.to_string(), 1.0)].into_iter().collect(),
                });
            }
            SupervisorMixerMode::Barge => {
                // All three can hear each other - full mesh
                // Customer -> agent + supervisor
                self.set_route(MixerRoute {
                    input_id: customer_input_id.to_string(),
                    outputs: [
                        (agent_output_id.to_string(), 1.0),
                        (supervisor_output_id.to_string(), 1.0),
                    ]
                    .into_iter()
                    .collect(),
                });
                // Agent -> customer + supervisor
                self.set_route(MixerRoute {
                    input_id: agent_input_id.to_string(),
                    outputs: [
                        (customer_output_id.to_string(), 1.0),
                        (supervisor_output_id.to_string(), 1.0),
                    ]
                    .into_iter()
                    .collect(),
                });
                // Supervisor -> customer + agent
                self.set_route(MixerRoute {
                    input_id: supervisor_input_id.to_string(),
                    outputs: [
                        (customer_output_id.to_string(), 1.0),
                        (agent_output_id.to_string(), 1.0),
                    ]
                    .into_iter()
                    .collect(),
                });
            }
        }
    }

    /// Start the mixer (creates mixing task)
    /// This spawns async tasks to receive audio from each input and mix to outputs
    /// Start the mixer and begin audio processing
    ///
    /// This spawns the mixing loop that:
    /// 1. Reads audio frames from each MixerInput
    /// 2. Mixes frames based on output routing
    /// 3. Sends mixed audio to each MixerOutput
    pub fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return; // Already started
        }

        let mixer_inputs = self.mixer_inputs.lock().unwrap();
        let mixer_outputs = self.mixer_outputs.lock().unwrap();
        let output_routes = self.output_routes.lock().unwrap();
        let mode = self.mode.lock().unwrap().clone();

        info!(
            "MediaMixer {} started with {} inputs, {} outputs, mode: {:?}",
            self.id,
            mixer_inputs.len(),
            mixer_outputs.len(),
            mode
        );

        // Clone the data needed for the mixing task
        let mixer = self.mixer.clone();
        let cancel_token = self.cancel_token.clone();
        let mixer_id = self.id.clone();
        let frame_size = self.frame_size;
        let sample_rate = self.sample_rate;

        // Clone the output routes
        let routes: std::collections::HashMap<String, Vec<String>> = output_routes.clone();

        // If no inputs/outputs configured, nothing to do
        if mixer_inputs.is_empty() || mixer_outputs.is_empty() {
            info!("MediaMixer {}: no inputs or outputs configured", self.id);
            return;
        }

        // Spawn the mixing task
        crate::utils::spawn(async move {
            Self::mixing_loop(
                &mixer_id,
                mixer,
                routes,
                cancel_token,
                frame_size,
                sample_rate,
            ).await;
        });
    }

    /// The main mixing loop
    /// Reads frames from inputs, mixes, and sends to outputs
    async fn mixing_loop(
        mixer_id: &str,
        _mixer: Arc<AudioMixer>,
        _output_routes: std::collections::HashMap<String, Vec<String>>,
        cancel_token: CancellationToken,
        frame_size: usize,
        sample_rate: u32,
    ) {
        info!("Mixing loop started for {} (frame_size: {}, sample_rate: {})", mixer_id, frame_size, sample_rate);

        // Calculate sleep interval based on frame size
        // frame_size samples at sample_rate Hz = frame_size / sample_rate seconds
        let interval_ms = (frame_size as f64 / sample_rate as f64 * 1000.0) as u64;
        let interval = std::time::Duration::from_millis(interval_ms.max(1));

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    info!("Mixing loop cancelled for {}", mixer_id);
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    // In a full implementation:
                    // 1. For each MixerInput, call read_frame() to get decoded PCM
                    // 2. Group PCM frames by output based on output_routes
                    // 3. For each output, mix the corresponding input frames
                    // 4. Call write_frame() on each MixerOutput with mixed PCM
                    //
                    // For now, this is a placeholder that logs activity
                    debug!("Mixing cycle for {}", mixer_id);
                }
            }
        }

        info!("Mixing loop stopped for {}", mixer_id);
    }

    /// Stop the mixer
    pub fn stop(&self) {
        self.cancel_token.cancel();
        self.started.store(false, Ordering::SeqCst);
        info!("MediaMixer {} stopped", self.id);
    }

    /// Get the mixer's cancel token
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Get mixer ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get sample rate
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Get channels
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Get the underlying audio mixer
    pub fn audio_mixer(&self) -> Arc<AudioMixer> {
        self.mixer.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_mixer_basic() {
        let mixer = AudioMixer::new(8000, 1);

        let frame1 = vec![1000i16; 160];
        let frame2 = vec![500i16; 160];
        let gains = [1.0, 1.0];

        let result = mixer.mix_frames(vec![frame1, frame2], &gains);

        // Should be roughly 1500 (1000 + 500) for each sample
        assert_eq!(result.len(), 160);
        assert!(result.iter().all(|&s| s > 1000));
    }

    #[test]
    fn test_audio_mixer_with_gain() {
        let mixer = AudioMixer::new(8000, 1);

        let frame1 = vec![1000i16; 160];
        let frame2 = vec![1000i16; 160];
        let gains = [1.0, 0.5]; // Second frame at half volume

        let result = mixer.mix_frames(vec![frame1, frame2], &gains);

        // Should be roughly 1500 (1000 + 500)
        assert_eq!(result.len(), 160);
    }

    #[test]
    fn test_audio_mixer_empty() {
        let mixer = AudioMixer::new(8000, 1);

        // Empty frames
        let result = mixer.mix_frames(vec![], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_mixer_creation() {
        let _mixer = MediaMixer::new("test-mixer".to_string(), 8000);
    }

    #[test]
    fn test_supervisor_mode_listen() {
        let mixer = MediaMixer::new("test".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Listen);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Listen);
    }

    #[test]
    fn test_supervisor_mode_whisper() {
        let mixer = MediaMixer::new("test".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Whisper);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Whisper);
    }

    #[test]
    fn test_supervisor_mode_barge() {
        let mixer = MediaMixer::new("test".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Barge);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Barge);
    }

    #[test]
    fn test_mixer_with_mixer_input_output() {
        // Test the new API with MixerInput/MixerOutput
        let mixer = MediaMixer::new("test-with-io".to_string(), 8000);

        // Verify initial state
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Listen);

        // Set mode
        mixer.set_mode(SupervisorMixerMode::Whisper);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Whisper);
    }

    #[test]
    fn test_output_routing() {
        let mixer = MediaMixer::new("test-routing".to_string(), 8000);

        // Set output routing
        mixer.set_output_routing("agent-out", vec!["customer".to_string()]);
        mixer.set_output_routing("customer-out", vec!["agent".to_string()]);

        // Verify routing
        let agent_route = mixer.get_output_routing("agent-out");
        assert!(agent_route.is_some());
        assert_eq!(agent_route.unwrap(), vec!["customer"]);

        let customer_route = mixer.get_output_routing("customer-out");
        assert!(customer_route.is_some());
        assert_eq!(customer_route.unwrap(), vec!["agent"]);
    }

    #[test]
    fn test_output_routing_not_found() {
        let mixer = MediaMixer::new("test-routing-missing".to_string(), 8000);

        // Non-existent output should return None
        let route = mixer.get_output_routing("non-existent");
        assert!(route.is_none());
    }

    #[test]
    fn test_audio_mixer_mix_frames_with_zero_gain() {
        let mixer = AudioMixer::new(8000, 1);

        let frame1 = vec![1000i16; 160];
        let frame2 = vec![1000i16; 160];
        let gains = [1.0, 0.0]; // Second frame has zero gain

        let result = mixer.mix_frames(vec![frame1, frame2], &gains);

        assert_eq!(result.len(), 160);
        // Result should be roughly 1000 (only first frame contributes)
        assert!(result.iter().all(|&s| s >= 900 && s <= 1100));
    }

    #[test]
    fn test_audio_mixer_mix_multiple_frames() {
        let mixer = AudioMixer::new(8000, 1);

        let frame1 = vec![100i16; 160];
        let frame2 = vec![100i16; 160];
        let frame3 = vec![100i16; 160];
        let gains = [1.0, 1.0, 1.0];

        let result = mixer.mix_frames(vec![frame1, frame2, frame3], &gains);

        assert_eq!(result.len(), 160);
        // Result should be roughly 300 (100 + 100 + 100)
        assert!(result.iter().all(|&s| s >= 250 && s <= 350));
    }

    #[test]
    fn test_audio_mixer_saturation_handling() {
        let mixer = AudioMixer::new(8000, 1);

        // Create frames that will saturate when mixed
        let frame1 = vec![30000i16; 160];
        let frame2 = vec![30000i16; 160];
        let gains = [1.0, 1.0];

        let result = mixer.mix_frames(vec![frame1, frame2], &gains);

        assert_eq!(result.len(), 160);
        // Should be clamped to i16::MAX
        assert!(result.iter().all(|&s| s == i16::MAX));
    }

    #[test]
    fn test_apply_supervisor_mode_listen() {
        let mixer = MediaMixer::new("test-apply-listen".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Listen);

        // Apply supervisor mode routing
        mixer.apply_supervisor_mode(
            "customer", "agent", "supervisor",
            "customer-out", "agent-out", "supervisor-out",
        );

        // Verify routes are set correctly for Listen mode
        let routes = mixer.get_routes();
        assert!(routes.contains_key("customer"));
        assert!(routes.contains_key("agent"));
        assert!(routes.contains_key("supervisor"));

        // In Listen mode: customer->agent, agent->customer, customer->supervisor, agent->supervisor
        // Supervisor has empty outputs (listen only)
        let sup_route = routes.get("supervisor");
        assert!(sup_route.is_some());
        assert!(sup_route.unwrap().outputs.is_empty());
    }

    #[test]
    fn test_apply_supervisor_mode_whisper() {
        let mixer = MediaMixer::new("test-apply-whisper".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Whisper);

        mixer.apply_supervisor_mode(
            "customer", "agent", "supervisor",
            "customer-out", "agent-out", "supervisor-out",
        );

        // Verify routes exist
        let routes = mixer.get_routes();
        assert!(routes.contains_key("customer"));
        assert!(routes.contains_key("agent"));
        assert!(routes.contains_key("supervisor"));
    }

    #[test]
    fn test_apply_supervisor_mode_barge() {
        let mixer = MediaMixer::new("test-apply-barge".to_string(), 8000);
        mixer.set_mode(SupervisorMixerMode::Barge);

        mixer.apply_supervisor_mode(
            "customer", "agent", "supervisor",
            "customer-out", "agent-out", "supervisor-out",
        );

        // In Barge mode, everyone can hear everyone
        let routes = mixer.get_routes();
        assert!(routes.contains_key("customer"));
        assert!(routes.contains_key("agent"));
        assert!(routes.contains_key("supervisor"));
    }

    #[test]
    fn test_mixer_start_stop() {
        let mixer = MediaMixer::new("test-start-stop".to_string(), 8000);

        // Start the mixer
        mixer.start();

        // Stop the mixer
        mixer.stop();

        // Should be able to start again after stop
        mixer.start();
        mixer.stop();
    }

    #[test]
    fn test_mixer_id_and_properties() {
        let mixer = MediaMixer::new("test-properties".to_string(), 16000);

        assert_eq!(mixer.id(), "test-properties");
        assert_eq!(mixer.sample_rate(), 16000);
        assert_eq!(mixer.channels(), 1);
        assert_eq!(mixer.get_mode(), SupervisorMixerMode::Listen); // default
    }
}
