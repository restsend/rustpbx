use audio_codec::{CodecType, Decoder, Encoder, Resampler, create_decoder, create_encoder};
use rustrtc::media::AudioFrame;

pub struct Transcoder {
    decoder: Box<dyn Decoder>,
    encoder: Box<dyn Encoder>,
    payload_type: u8,
    resampler: Option<Resampler>,
    rtp_timestamp: u32,
    sequence_number: u16,
}

impl Transcoder {
    pub fn new(from: CodecType, to: CodecType) -> Self {
        let decoder = create_decoder(from);
        let encoder = create_encoder(to);

        let source_sample_rate = decoder.sample_rate();
        let target_sample_rate = encoder.sample_rate();
        let payload_type = to.payload_type();
        let resampler = if source_sample_rate != target_sample_rate {
            Some(Resampler::new(
                source_sample_rate as usize,
                target_sample_rate as usize,
            ))
        } else {
            None
        };
        Self {
            decoder,
            encoder,
            resampler,
            payload_type,
            rtp_timestamp: 0,
            sequence_number: 0,
        }
    }

    pub fn transcode(&mut self, frame: &AudioFrame) -> AudioFrame {
        let mut pcmbuf = self.decoder.decode(&frame.data);
        if let Some(resampler) = &mut self.resampler {
            pcmbuf = resampler.resample(&pcmbuf);
        }
        let samples = pcmbuf.len() as u32;
        let encoded_data = self.encoder.encode(&pcmbuf);

        // Save current timestamp before incrementing
        let current_ts = self.rtp_timestamp;
        self.rtp_timestamp = self.rtp_timestamp.wrapping_add(samples);

        let current_seq = self.sequence_number;
        self.sequence_number = self.sequence_number.wrapping_add(1);

        AudioFrame {
            rtp_timestamp: current_ts,  // Use timestamp before increment
            sample_rate: self.encoder.sample_rate(),
            channels: self.encoder.channels() as u8,
            samples,
            data: encoded_data.into(),
            sequence_number: Some(current_seq), // Set sequence number to signal app control
            payload_type: Some(self.payload_type),
        }
    }
}
