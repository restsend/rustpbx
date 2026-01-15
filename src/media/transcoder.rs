use audio_codec::{CodecType, Decoder, Encoder, Resampler, create_decoder, create_encoder};
use rand::Rng;
use rustrtc::media::AudioFrame;

pub struct Transcoder {
    decoder: Box<dyn Decoder>,
    encoder: Box<dyn Encoder>,
    target: CodecType,
    resampler: Option<Resampler>,
    first_input_timestamp: Option<u32>,
    first_input_sequence: Option<u16>,
    first_output_timestamp: u32,
    first_output_sequence: u16,
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

        let mut rng = rand::rng();
        let first_output_timestamp: u32 = rng.random();
        let first_output_sequence: u16 = rng.random();

        Self {
            decoder,
            encoder,
            resampler,
            target,
            first_input_timestamp: None,
            first_input_sequence: None,
            first_output_timestamp,
            first_output_sequence,
        }
    }

    pub fn transcode(&mut self, frame: &AudioFrame) -> AudioFrame {
        if self.first_input_timestamp.is_none() {
            self.first_input_timestamp = Some(frame.rtp_timestamp);
            self.first_input_sequence = frame.sequence_number;
        }

        let mut pcmbuf = self.decoder.decode(&frame.data);
        if let Some(resampler) = &mut self.resampler {
            pcmbuf = resampler.resample(&pcmbuf);
        }

        let encoded_data = self.encoder.encode(&pcmbuf);

        let first_input_ts = self.first_input_timestamp.unwrap_or(0);
        let input_ts_delta = frame.rtp_timestamp.wrapping_sub(first_input_ts);
        let input_clock_rate = frame.clock_rate;
        let target_clock_rate = self.target.clock_rate();
        let output_ts_delta =
            (input_ts_delta as u64 * target_clock_rate as u64 / input_clock_rate as u64) as u32;
        let output_timestamp = self.first_output_timestamp.wrapping_add(output_ts_delta);

        let first_input_seq = self.first_input_sequence.unwrap_or(0);
        let input_seq = frame.sequence_number.unwrap_or(0);
        let seq_delta = input_seq.wrapping_sub(first_input_seq);
        let output_sequence = self.first_output_sequence.wrapping_add(seq_delta);

        AudioFrame {
            rtp_timestamp: output_timestamp,
            clock_rate: self.target.clock_rate(),
            data: encoded_data.into(),
            sequence_number: Some(output_sequence),
            payload_type: Some(self.target.payload_type()),
        }
    }
}
