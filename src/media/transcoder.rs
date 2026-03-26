use audio_codec::{CodecType, Decoder, Encoder, Resampler, create_decoder, create_encoder};
use rand::RngExt;
use rustrtc::media::AudioFrame;

#[derive(Clone, Copy)]
struct TimestampDomain {
    first_input_timestamp: u32,
    first_output_timestamp: u32,
}

pub struct RtpTiming {
    domain: Option<TimestampDomain>,
    first_input_sequence: Option<u16>,
    first_output_timestamp: u32,
    first_output_sequence: u16,
}

impl Default for RtpTiming {
    fn default() -> Self {
        let mut rng = rand::rng();
        Self {
            domain: None,
            first_input_sequence: None,
            first_output_timestamp: rng.random(),
            first_output_sequence: rng.random(),
        }
    }
}

impl RtpTiming {
    pub fn rewrite(
        &mut self,
        frame: &mut AudioFrame,
        source_clock_rate: u32,
        target_clock_rate: u32,
        target_payload_type: u8,
    ) {
        let (rtp_timestamp, output_sequence) =
            self.new_timestamp(frame, source_clock_rate, target_clock_rate);
        frame.rtp_timestamp = rtp_timestamp;
        frame.sequence_number = Some(output_sequence);
        frame.payload_type = Some(target_payload_type);
        frame.clock_rate = target_clock_rate;
    }

    fn new_timestamp(
        &mut self,
        frame: &AudioFrame,
        source_clock_rate: u32,
        target_clock_rate: u32,
    ) -> (u32, u16) {
        if self.first_input_sequence.is_none() {
            self.first_input_sequence = frame.sequence_number;
        }

        let domain = self.domain.get_or_insert(TimestampDomain {
            first_input_timestamp: frame.rtp_timestamp,
            first_output_timestamp: self.first_output_timestamp,
        });

        let input_ts_delta = frame
            .rtp_timestamp
            .wrapping_sub(domain.first_input_timestamp);
        let output_ts_delta =
            (input_ts_delta as u64 * target_clock_rate as u64 / source_clock_rate as u64) as u32;
        let output_timestamp = domain.first_output_timestamp.wrapping_add(output_ts_delta);

        let first_input_seq = self.first_input_sequence.unwrap_or_default();
        let input_seq = frame.sequence_number.unwrap_or_default();
        let seq_delta = input_seq.wrapping_sub(first_input_seq);
        let output_sequence = self.first_output_sequence.wrapping_add(seq_delta);

        (output_timestamp, output_sequence)
    }
}

/// Rewrite the duration field inside a telephone-event (RFC 4733) payload.
/// Duration is bytes [2..4] in network byte order, expressed in RTP clock ticks.
pub fn rewrite_dtmf_duration(data: &[u8], source_rate: u32, target_rate: u32) -> bytes::Bytes {
    if data.len() < 4 || source_rate == target_rate {
        return bytes::Bytes::copy_from_slice(data);
    }
    let mut buf = data.to_vec();
    let duration = u16::from_be_bytes([buf[2], buf[3]]);
    let scaled = (duration as u32 * target_rate / source_rate) as u16;
    buf[2..4].copy_from_slice(&scaled.to_be_bytes());
    bytes::Bytes::from(buf)
}

pub struct Transcoder {
    decoder: Box<dyn Decoder>,
    encoder: Box<dyn Encoder>,
    source: CodecType,
    target: CodecType,
    /// The actual negotiated PT for the target codec (from SDP answer, not codec default)
    target_pt: u8,
    resampler: Option<Resampler>,
}

impl Transcoder {
    pub fn new(source: CodecType, target: CodecType, target_pt: u8) -> Self {
        let decoder = create_decoder(source);
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
            source,
            target,
            target_pt,
            resampler,
        }
    }

    pub fn source_clock_rate(&self) -> u32 {
        self.source.clock_rate()
    }

    pub fn target_clock_rate(&self) -> u32 {
        self.target.clock_rate()
    }

    pub fn transcode(&mut self, frame: &AudioFrame) -> AudioFrame {
        let mut pcmbuf = self.decoder.decode(&frame.data);
        if let Some(resampler) = &mut self.resampler {
            pcmbuf = resampler.resample(&pcmbuf);
        }

        let encoded_data = self.encoder.encode(&pcmbuf);

        AudioFrame {
            rtp_timestamp: frame.rtp_timestamp,
            clock_rate: self.target.clock_rate(),
            data: encoded_data.into(),
            sequence_number: frame.sequence_number,
            payload_type: Some(self.target_pt),
            marker: frame.marker,
            raw_packet: None,
            source_addr: frame.source_addr,
        }
    }
}
