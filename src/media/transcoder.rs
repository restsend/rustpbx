use audio_codec::{CodecType, Decoder, Encoder, Resampler, create_decoder, create_encoder};
use rustrtc::media::AudioFrame;

pub struct Transcoder {
    decoder: Box<dyn Decoder>,
    encoder: Box<dyn Encoder>,
    target: CodecType,
    resampler: Option<Resampler>,
    rtp_timestamp: u32,
    sequence_number: u16,
}

impl Transcoder {
    pub fn new(from: CodecType, target: CodecType) -> Self {
        let decoder = create_decoder(from);
        let encoder = create_encoder(target);

        let source_sample_rate = decoder.sample_rate();
        let target_sample_rate = encoder.sample_rate();
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
            target,
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

        let encoder_sample_rate = self.encoder.sample_rate();
        let target_clock_rate = self.target.clock_rate();
        let timestamp_increment = samples * target_clock_rate / encoder_sample_rate;

        let current_ts = self.rtp_timestamp;
        self.rtp_timestamp = self.rtp_timestamp.wrapping_add(timestamp_increment);
        let current_seq = self.sequence_number;
        self.sequence_number = self.sequence_number.wrapping_add(1);

        AudioFrame {
            rtp_timestamp: current_ts, // Use timestamp before increment
            clock_rate: self.target.clock_rate(),
            data: encoded_data.into(),
            sequence_number: Some(current_seq), // Set sequence number to signal app control
            payload_type: Some(self.target.payload_type()),
        }
    }
}
