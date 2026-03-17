use crate::proxy::proxy_call::media_peer::MediaPeer;
use audio_codec::{CodecType, Decoder};
use std::sync::Arc;

/// Input source for the audio mixer
/// Handles receiving RTP audio frames from a MediaPeer and decoding to PCM
pub struct MixerInput {
    /// Unique identifier for this input
    pub id: String,
    /// The MediaPeer source
    peer: Arc<dyn MediaPeer>,
    /// Audio codec used by this input
    codec: CodecType,
    /// Decoder instance (created on demand)
    decoder: Option<Box<dyn Decoder + Send>>,
    /// Track ID to read from
    track_id: Option<String>,
}

impl MixerInput {
    pub fn new(id: String, peer: Arc<dyn MediaPeer>, codec: CodecType) -> Self {
        Self {
            id,
            peer,
            codec,
            decoder: None,
            track_id: None,
        }
    }

    /// Get the codec for this input
    pub fn codec(&self) -> CodecType {
        self.codec
    }

    /// Set the track ID to read from
    pub fn with_track(mut self, track_id: String) -> Self {
        self.track_id = Some(track_id);
        self
    }

    /// Initialize the decoder if not already done
    fn ensure_decoder(&mut self) {
        if self.decoder.is_none() {
            self.decoder = Some(audio_codec::create_decoder(self.codec));
        }
    }

    /// Read and decode one audio frame from the input
    /// Returns the decoded PCM samples (16-bit signed)
    pub async fn read_frame(&mut self) -> Option<DecodedFrame> {
        // Ensure decoder is created
        self.ensure_decoder();

        let tracks = self.peer.get_tracks().await;

        if tracks.is_empty() {
            return None;
        }

        // Select track based on track_id if specified
        let track = if let Some(ref track_id) = self.track_id {
            tracks.iter().find(|t| {
                let guard = t.blocking_lock();
                guard.id() == track_id
            }).cloned()
        } else {
            tracks.first().cloned()
        }?;

        // Try to receive a frame from the track
        let _track_guard = track.lock().await;

        // Check if there's an audio frame available (non-blocking)
        // Note: In a full implementation, we'd use a proper async receive method
        // For now, we'll return None if no frame is ready

        // The track doesn't have a non-blocking recv, so we return None
        // In production, this would be handled by the mixing loop with proper async
        None
    }

    /// Try to decode encoded audio data
    /// Returns decoded PCM samples
    pub fn decode(&mut self, encoded_data: &[u8]) -> Option<Vec<i16>> {
        self.ensure_decoder();
        if let Some(ref mut decoder) = self.decoder {
            Some(decoder.decode(encoded_data))
        } else {
            None
        }
    }

    /// Get the sample rate of the decoder
    pub fn sample_rate(&self) -> u32 {
        self.decoder.as_ref().map(|d| d.sample_rate()).unwrap_or(8000)
    }

    /// Create a decoder for the given codec
    pub fn create_decoder(codec: CodecType) -> Box<dyn Decoder + Send> {
        audio_codec::create_decoder(codec)
    }
}

/// A frame of decoded audio ready for mixing
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Input ID this frame came from
    pub input_id: String,
    /// PCM samples (16-bit signed, mono)
    pub samples: Vec<i16>,
    /// Sample rate
    pub sample_rate: u32,
    /// Timestamp for synchronization
    pub timestamp: u64,
    /// Sequence number for ordering
    pub sequence: Option<u16>,
}

impl DecodedFrame {
    pub fn new(input_id: String, samples: Vec<i16>, sample_rate: u32, timestamp: u64) -> Self {
        Self {
            input_id,
            samples,
            sample_rate,
            timestamp,
            sequence: None,
        }
    }

    pub fn with_sequence(mut self, seq: u16) -> Self {
        self.sequence = Some(seq);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::proxy_call::test_util::tests::MockMediaPeer;

    #[test]
    fn test_mixer_input_creation() {
        let peer = Arc::new(MockMediaPeer::new());
        let input = MixerInput::new("input-1".to_string(), peer, CodecType::PCMU);

        assert_eq!(input.id, "input-1");
        assert_eq!(input.codec(), CodecType::PCMU);
    }

    #[test]
    fn test_mixer_input_with_track() {
        let peer = Arc::new(MockMediaPeer::new());
        let input = MixerInput::new("input-1".to_string(), peer, CodecType::PCMU)
            .with_track("audio-0".to_string());

        // Track ID should be set
        assert_eq!(input.track_id, Some("audio-0".to_string()));
    }

    #[test]
    fn test_mixer_input_ensure_decoder() {
        let peer = Arc::new(MockMediaPeer::new());
        let mut input = MixerInput::new("input-1".to_string(), peer, CodecType::PCMU);

        // Decoder should be None initially
        assert!(input.decoder.is_none());

        // Call ensure_decoder
        input.ensure_decoder();

        // Decoder should now be created
        assert!(input.decoder.is_some());
    }

    #[test]
    fn test_mixer_input_ensure_decoder_opus() {
        let peer = Arc::new(MockMediaPeer::new());
        let mut input = MixerInput::new("input-1".to_string(), peer, CodecType::Opus);

        input.ensure_decoder();

        assert!(input.decoder.is_some());
    }

    #[test]
    fn test_mixer_input_sample_rate() {
        let peer = Arc::new(MockMediaPeer::new());
        let mut input = MixerInput::new("input-1".to_string(), peer, CodecType::PCMU);

        // Before decoder is created, should return default
        assert_eq!(input.sample_rate(), 8000);

        // After decoder is created, should return actual sample rate
        input.ensure_decoder();
        // PCMU decoder should have 8kHz sample rate
        assert_eq!(input.sample_rate(), 8000);
    }

    #[test]
    fn test_decoded_frame() {
        let frame = DecodedFrame::new(
            "test-input".to_string(),
            vec![0i16; 160],
            8000,
            0,
        );

        assert_eq!(frame.input_id, "test-input");
        assert_eq!(frame.samples.len(), 160);
        assert_eq!(frame.sample_rate, 8000);
    }

    #[test]
    fn test_decoded_frame_with_sequence() {
        let frame = DecodedFrame::new(
            "test-input".to_string(),
            vec![1i16, 2i16, 3i16],
            8000,
            100,
        ).with_sequence(42);

        assert_eq!(frame.sequence, Some(42));
        assert_eq!(frame.timestamp, 100);
    }
}
