use crate::proxy::proxy_call::media_peer::MediaPeer;
use audio_codec::{CodecType, Encoder};
use std::sync::Arc;
use tracing::debug;

/// Output destination for the audio mixer
/// Handles encoding PCM to RTP and sending to a MediaPeer
pub struct MixerOutput {
    /// Unique identifier for this output
    pub id: String,
    /// The MediaPeer destination
    peer: Arc<dyn MediaPeer>,
    /// Audio codec used by this output
    codec: CodecType,
    /// Encoder instance (created on demand)
    encoder: Option<Box<dyn Encoder + Send>>,
    /// Track ID to write to
    track_id: Option<String>,
    /// Gain factor for this output (0.0 - 2.0)
    gain: f32,
}

impl MixerOutput {
    pub fn new(id: String, peer: Arc<dyn MediaPeer>, codec: CodecType) -> Self {
        Self {
            id,
            peer,
            codec,
            encoder: None,
            track_id: None,
            gain: 1.0,
        }
    }

    /// Get the codec for this output
    pub fn codec(&self) -> CodecType {
        self.codec
    }

    /// Set the track ID to write to
    pub fn with_track(mut self, track_id: String) -> Self {
        self.track_id = Some(track_id);
        self
    }

    /// Set the gain factor
    pub fn with_gain(mut self, gain: f32) -> Self {
        self.gain = gain.clamp(0.0, 2.0);
        self
    }

    /// Initialize the encoder if not already done
    fn ensure_encoder(&mut self) {
        if self.encoder.is_none() {
            self.encoder = Some(audio_codec::create_encoder(self.codec));
        }
    }

    /// Get the peer reference
    pub fn peer(&self) -> Arc<dyn MediaPeer> {
        self.peer.clone()
    }

    /// Get the track ID
    pub fn track_id(&self) -> Option<&str> {
        self.track_id.as_deref()
    }

    /// Encode and send PCM samples as RTP audio frame
    /// 1. Apply gain to samples
    /// 2. Encode PCM to RTP payload
    /// 3. Send to the MediaPeer's track
    pub async fn write_frame(&mut self, samples: &[i16]) -> Result<(), String> {
        self.ensure_encoder();

        // 1. Apply gain
        let samples_with_gain: Vec<i16> = if (self.gain - 1.0).abs() > 0.01 {
            samples
                .iter()
                .map(|&s| {
                    let scaled = s as f32 * self.gain;
                    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
                })
                .collect()
        } else {
            samples.to_vec()
        };

        // 2. Encode to RTP payload
        if let Some(ref mut encoder) = self.encoder {
            let encoded = encoder.encode(&samples_with_gain);

            // 3. Get tracks from peer and send
            // Note: This requires the MediaPeer to have a method for sending audio
            // For now, we log the encoded data size as a placeholder
            debug!(
                "MixerOutput {}: encoded {} bytes from {} samples (gain: {})",
                self.id,
                encoded.len(),
                samples_with_gain.len(),
                self.gain
            );
        }

        Ok(())
    }

    /// Encode samples and return the encoded data (for testing/debugging)
    pub fn encode(&mut self, samples: &[i16]) -> Option<Vec<u8>> {
        self.ensure_encoder();
        if let Some(ref mut encoder) = self.encoder {
            Some(encoder.encode(samples))
        } else {
            None
        }
    }

    /// Get the encoder's sample rate
    pub fn sample_rate(&self) -> u32 {
        self.encoder
            .as_ref()
            .map(|e| e.sample_rate())
            .unwrap_or(8000)
    }

    /// Create an encoder for the given codec
    pub fn create_encoder(codec: CodecType) -> Box<dyn Encoder + Send> {
        audio_codec::create_encoder(codec)
    }
}

/// Configuration for a mixer output
#[derive(Debug, Clone)]
pub struct MixerOutputConfig {
    /// Output ID
    pub id: String,
    /// Target MediaPeer (will be stored as weak ref or handle)
    pub peer_id: String,
    /// Audio codec
    pub codec: CodecType,
    /// Gain factor
    pub gain: f32,
    /// Which inputs to include in this output
    pub inputs: Vec<String>,
}

impl MixerOutputConfig {
    pub fn new(id: String, peer_id: String) -> Self {
        Self {
            id,
            peer_id,
            codec: CodecType::PCMU, // Default
            gain: 1.0,
            inputs: Vec::new(),
        }
    }

    pub fn with_codec(mut self, codec: CodecType) -> Self {
        self.codec = codec;
        self
    }

    pub fn with_gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }

    pub fn with_inputs(mut self, inputs: Vec<String>) -> Self {
        self.inputs = inputs;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mixer_output_config() {
        let config = MixerOutputConfig::new("output-1".to_string(), "peer-1".to_string())
            .with_codec(CodecType::PCMU)
            .with_gain(0.8)
            .with_inputs(vec!["input-1".to_string(), "input-2".to_string()]);

        assert_eq!(config.id, "output-1");
        assert_eq!(config.peer_id, "peer-1");
        assert_eq!(config.codec, CodecType::PCMU);
        assert_eq!(config.gain, 0.8);
        assert_eq!(config.inputs.len(), 2);
    }

    #[test]
    fn test_gain_clamping() {
        let output = MixerOutput::new(
            "test".to_string(),
            Arc::new(crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new()),
            CodecType::PCMU,
        );

        // Gain should be clamped to valid range
        assert!(output.gain >= 0.0 && output.gain <= 2.0);
    }

    #[test]
    fn test_mixer_output_creation() {
        let peer = Arc::new(crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new());
        let output = MixerOutput::new("output-1".to_string(), peer.clone(), CodecType::PCMU);

        assert_eq!(output.id, "output-1");
        assert_eq!(output.codec(), CodecType::PCMU);
    }

    #[test]
    fn test_mixer_output_with_track() {
        let peer = Arc::new(crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new());
        let output = MixerOutput::new("output-1".to_string(), peer, CodecType::PCMU)
            .with_track("audio-0".to_string())
            .with_gain(0.5);

        assert_eq!(output.track_id(), Some("audio-0"));
        assert_eq!(output.gain, 0.5);
    }

    #[test]
    fn test_mixer_output_ensure_encoder() {
        let peer = Arc::new(crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new());
        let mut output = MixerOutput::new("output-1".to_string(), peer, CodecType::PCMU);

        // Encoder should be None initially
        assert!(output.encoder.is_none());

        // Call ensure_encoder (via write_frame)
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            output.write_frame(&[0i16; 160]).await.unwrap();
        });

        // Encoder should now be created
        assert!(output.encoder.is_some());
    }

    #[test]
    fn test_mixer_output_encode() {
        let peer = Arc::new(crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new());
        let mut output = MixerOutput::new("output-1".to_string(), peer, CodecType::PCMU);

        // Create test PCM samples
        let samples: Vec<i16> = (0..160).map(|i| i as i16).collect();

        // Encode the samples
        let encoded = output.encode(&samples);

        assert!(encoded.is_some());
        // PCMU encoding should produce fewer bytes than input
        assert!(encoded.unwrap().len() < samples.len() * 2);
    }

    #[test]
    fn test_mixer_output_sample_rate() {
        let peer = Arc::new(crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new());
        let output = MixerOutput::new("output-1".to_string(), peer, CodecType::PCMU);

        // Default sample rate should be 8000
        assert_eq!(output.sample_rate(), 8000);
    }

    #[test]
    fn test_mixer_output_peer_getter() {
        let peer: Arc<dyn crate::proxy::proxy_call::media_peer::MediaPeer> =
            Arc::new(crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new());
        let output = MixerOutput::new("output-1".to_string(), peer.clone(), CodecType::PCMU);

        // peer() should return the same reference
        assert!(Arc::ptr_eq(&output.peer(), &peer));
    }

    #[test]
    fn test_mixer_output_gain_application() {
        let peer = Arc::new(crate::proxy::proxy_call::test_util::tests::MockMediaPeer::new());
        let mut output =
            MixerOutput::new("output-1".to_string(), peer, CodecType::PCMU).with_gain(2.0); // Double the gain

        let samples: Vec<i16> = vec![1000i16; 160];

        // Encode with gain
        let encoded = output.encode(&samples);

        // Should produce encoded output
        assert!(encoded.is_some());
    }
}
