pub mod silero;
pub mod webrtc;

pub struct VADConfig {
    /// Minimum duration of silence to consider speech ended (in milliseconds)
    pub silence_duration_threshold: u64,
    /// Duration of audio to keep before speech starts (in milliseconds)
    /// - Pre-speech padding (150ms):
    /// - Captures speech onset including plosive sounds (like 'p', 'b', 't')
    /// - Helps preserve the natural beginning of utterances
    /// - Typical plosive onset is 50-100ms, so 150ms gives some margin
    pub pre_speech_padding: u64,
    /// Duration of audio to keep after speech ends (in milliseconds)
    /// Post-speech padding (150ms):
    // - Equal to pre-speech for symmetry
    // - Sufficient to capture trailing sounds and natural decay
    // - Avoids cutting off final consonants
    pub post_speech_padding: u64,
    /// Threshold for voice activity detection (0.0 to 1.0)
    pub voice_threshold: f32,
}

impl Default for VADConfig {
    fn default() -> Self {
        Self {
            silence_duration_threshold: 800, // Longer pause detection (800ms)
            pre_speech_padding: 150,         // Keep 150ms before speech
            post_speech_padding: 200,        // Keep 200ms after speech
            voice_threshold: 0.5,
        }
    }
}
