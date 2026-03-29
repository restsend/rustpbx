use audio_codec::{CodecType, Decoder, Encoder, Resampler, create_decoder, create_encoder};
use rustrtc::media::AudioFrame;

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
            target,
            target_pt,
            resampler,
        }
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
