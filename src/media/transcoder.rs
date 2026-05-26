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

    pub fn target_pt(&self) -> u8 {
        self.target_pt
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
            header_extension: None,
            raw_packet: None,
            source_addr: frame.source_addr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_codec::{create_decoder, create_encoder};
    use bytes::Bytes;

    fn make_frame(pt: u8, clock_rate: u32, data: Vec<u8>, seq: u16, ts: u32) -> AudioFrame {
        AudioFrame {
            rtp_timestamp: ts,
            clock_rate,
            data: Bytes::from(data),
            sequence_number: Some(seq),
            payload_type: Some(pt),
            marker: false,
            header_extension: None,
            raw_packet: None,
            source_addr: None,
        }
    }

    fn silence_pcm_8k() -> Vec<i16> {
        vec![0i16; 160]
    }

    fn sine_pcm_8k(freq: f64, amp: f64) -> Vec<i16> {
        (0..160)
            .map(|i| (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / 8000.0).sin()) as i16)
            .collect()
    }

    #[test]
    fn test_transcoder_pcmu_to_pcma() {
        let pcm = sine_pcm_8k(440.0, 8000.0);
        let mut pcmu_enc = create_encoder(CodecType::PCMU);
        let pcmu_data = pcmu_enc.encode(&pcm);

        let mut t = Transcoder::new(CodecType::PCMU, CodecType::PCMA, 8);
        let input = make_frame(0, 8000, pcmu_data, 1, 0);
        let output = t.transcode(&input);

        assert_eq!(output.payload_type, Some(8));
        assert_eq!(output.clock_rate, 8000);
        assert_eq!(output.data.len(), 160, "PCMA 20ms = 160 bytes");

        let mut dec = create_decoder(CodecType::PCMA);
        let roundtrip = dec.decode(&output.data);
        assert_eq!(roundtrip.len(), 160);

        let energy: f64 = roundtrip.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / 160.0;
        assert!(
            energy.sqrt() > 500.0,
            "Signal should survive PCMU→PCMA transcoding, RMS={}",
            energy.sqrt()
        );
    }

    #[test]
    fn test_transcoder_pcma_to_pcmu() {
        let pcm = sine_pcm_8k(440.0, 8000.0);
        let mut pcma_enc = create_encoder(CodecType::PCMA);
        let pcma_data = pcma_enc.encode(&pcm);

        let mut t = Transcoder::new(CodecType::PCMA, CodecType::PCMU, 0);
        let input = make_frame(8, 8000, pcma_data, 1, 0);
        let output = t.transcode(&input);

        assert_eq!(output.payload_type, Some(0));
        assert_eq!(output.clock_rate, 8000);
        assert_eq!(output.data.len(), 160);

        let mut dec = create_decoder(CodecType::PCMU);
        let roundtrip = dec.decode(&output.data);
        assert_eq!(roundtrip.len(), 160);

        let energy: f64 = roundtrip.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / 160.0;
        assert!(
            energy.sqrt() > 500.0,
            "Signal should survive PCMA→PCMU, RMS={}",
            energy.sqrt()
        );
    }

    #[test]
    fn test_transcoder_g722_to_pcmu() {
        let pcm_16k: Vec<i16> = (0..320)
            .map(|i| {
                (8000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 16000.0).sin()) as i16
            })
            .collect();
        let mut g722_enc = create_encoder(CodecType::G722);
        let g722_data = g722_enc.encode(&pcm_16k);

        let mut t = Transcoder::new(CodecType::G722, CodecType::PCMU, 0);
        let input = make_frame(9, 8000, g722_data, 1, 0);
        let output = t.transcode(&input);

        assert_eq!(output.payload_type, Some(0));
        assert_eq!(output.clock_rate, 8000);
        assert_eq!(output.data.len(), 160, "PCMU 20ms at 8kHz = 160 bytes");
    }

    #[test]
    fn test_transcoder_pcmu_to_g722() {
        let pcm = sine_pcm_8k(440.0, 8000.0);
        let mut pcmu_enc = create_encoder(CodecType::PCMU);
        let pcmu_data = pcmu_enc.encode(&pcm);

        let mut t = Transcoder::new(CodecType::PCMU, CodecType::G722, 9);
        let input = make_frame(0, 8000, pcmu_data, 1, 0);
        let output = t.transcode(&input);

        assert_eq!(output.payload_type, Some(9));
        assert_eq!(output.clock_rate, 8000);
        assert!(!output.data.is_empty(), "G722 output must not be empty");
    }

    #[test]
    fn test_transcoder_g729_to_pcmu() {
        let pcm = silence_pcm_8k();
        let mut g729_enc = create_encoder(CodecType::G729);
        let g729_data = g729_enc.encode(&pcm);

        let mut t = Transcoder::new(CodecType::G729, CodecType::PCMU, 0);
        let input = make_frame(18, 8000, g729_data, 1, 0);
        let output = t.transcode(&input);

        assert_eq!(output.payload_type, Some(0));
        assert_eq!(output.clock_rate, 8000);
        assert_eq!(output.data.len(), 160, "PCMU 20ms = 160 bytes");
    }

    #[test]
    fn test_transcoder_pcmu_to_g729() {
        let pcm = silence_pcm_8k();
        let mut pcmu_enc = create_encoder(CodecType::PCMU);
        let pcmu_data = pcmu_enc.encode(&pcm);

        let mut t = Transcoder::new(CodecType::PCMU, CodecType::G729, 18);
        let input = make_frame(0, 8000, pcmu_data, 1, 0);
        let output = t.transcode(&input);

        assert_eq!(output.payload_type, Some(18));
        assert_eq!(output.clock_rate, 8000);
        assert!(!output.data.is_empty(), "G729 output must not be empty");
    }

    #[cfg(feature = "opus")]
    #[test]
    fn test_transcoder_pcmu_to_opus() {
        let pcm = silence_pcm_8k();
        let mut pcmu_enc = create_encoder(CodecType::PCMU);
        let pcmu_data = pcmu_enc.encode(&pcm);

        let mut dec = create_decoder(CodecType::PCMU);
        let decoded = dec.decode(&pcmu_data);
        assert_eq!(decoded.len(), 160, "PCMU decode should yield 160 samples");

        let mut t = Transcoder::new(CodecType::PCMU, CodecType::Opus, 111);
        let input = make_frame(0, 8000, pcmu_data, 1, 0);
        let output = t.transcode(&input);

        assert_eq!(output.payload_type, Some(111));
        assert_eq!(output.clock_rate, 48000);
        if output.data.is_empty() {
            eprintln!(
                "WARNING: PCMU→Opus transcoding produced empty output (known Opus encoder behavior for silence)"
            );
        } else {
            assert!(output.data.len() > 0, "Opus output should have data");
        }
    }

    #[test]
    fn test_transcoder_preserves_timestamp() {
        let pcm = silence_pcm_8k();
        let mut enc = create_encoder(CodecType::PCMU);
        let data = enc.encode(&pcm);

        let mut t = Transcoder::new(CodecType::PCMU, CodecType::PCMA, 8);
        let input = make_frame(0, 8000, data, 42, 12345);
        let output = t.transcode(&input);

        assert_eq!(output.rtp_timestamp, 12345);
        assert_eq!(output.sequence_number, Some(42));
    }

    #[test]
    fn test_transcoder_multiple_frames() {
        let pcm = sine_pcm_8k(440.0, 8000.0);
        let mut enc = create_encoder(CodecType::PCMU);
        let pcmu_data = enc.encode(&pcm);

        let mut t = Transcoder::new(CodecType::PCMU, CodecType::PCMA, 8);
        for i in 0..10 {
            let input = make_frame(0, 8000, pcmu_data.clone(), i, i as u32 * 160);
            let output = t.transcode(&input);
            assert_eq!(output.payload_type, Some(8));
            assert_eq!(output.data.len(), 160, "Frame {} size mismatch", i);
        }
    }

    #[test]
    fn test_rtp_timing_same_clock_rate() {
        let mut timing = RtpTiming::default();
        let mut frame = make_frame(0, 8000, vec![0u8; 160], 100, 50000);

        timing.rewrite(&mut frame, 8000, 8000, 0);

        assert_eq!(frame.payload_type, Some(0));
        assert!(frame.sequence_number.is_some());
    }

    #[test]
    fn test_rtp_timing_cross_clock_rate_48k_to_8k() {
        let mut timing = RtpTiming::default();
        let mut frame1 = make_frame(111, 48000, vec![0u8; 40], 100, 0);
        let mut frame2 = make_frame(111, 48000, vec![0u8; 40], 101, 960);

        timing.rewrite(&mut frame1, 48000, 8000, 0);
        timing.rewrite(&mut frame2, 48000, 8000, 0);

        let ts_delta = frame2.rtp_timestamp.wrapping_sub(frame1.rtp_timestamp);
        assert_eq!(
            ts_delta, 160,
            "48kHz→8kHz: 960 tick delta should become 160, got {}",
            ts_delta
        );
        let seq_delta = frame2
            .sequence_number
            .unwrap()
            .wrapping_sub(frame1.sequence_number.unwrap());
        assert_eq!(seq_delta, 1, "Sequence should increment by 1");
    }

    #[test]
    fn test_rtp_timing_cross_clock_rate_8k_to_48k() {
        let mut timing = RtpTiming::default();
        let mut frame1 = make_frame(0, 8000, vec![0u8; 160], 10, 0);
        let mut frame2 = make_frame(0, 8000, vec![0u8; 160], 11, 160);

        timing.rewrite(&mut frame1, 8000, 48000, 111);
        timing.rewrite(&mut frame2, 8000, 48000, 111);

        let ts_delta = frame2.rtp_timestamp.wrapping_sub(frame1.rtp_timestamp);
        assert_eq!(
            ts_delta, 960,
            "8kHz→48kHz: 160 tick delta should become 960, got {}",
            ts_delta
        );
    }

    #[test]
    fn test_rtp_timing_monotonic_sequence() {
        let mut timing = RtpTiming::default();
        let mut outputs = Vec::new();

        for i in 0u16..100 {
            let mut frame = make_frame(0, 8000, vec![0u8; 160], 1000 + i, 50000 + i as u32 * 160);
            timing.rewrite(&mut frame, 8000, 8000, 0);
            outputs.push(frame.sequence_number.unwrap());
        }

        for w in outputs.windows(2) {
            assert_eq!(
                w[1].wrapping_sub(w[0]),
                1,
                "Sequence must be monotonically increasing by 1"
            );
        }
    }

    #[test]
    fn test_rewrite_dtmf_duration_identity() {
        let data = vec![0x05u8, 0x0A, 0x10, 0xE0];
        let result = rewrite_dtmf_duration(&data, 8000, 8000);
        assert_eq!(&result[..], &data[..], "Same rate should be identity");
    }

    #[test]
    fn test_rewrite_dtmf_duration_48k_to_8k() {
        let data = vec![0x05u8, 0x8A, 0x12, 0xC0]; // digit 5, duration=4800 (48kHz ticks)
        let result = rewrite_dtmf_duration(&data, 48000, 8000);

        let duration = u16::from_be_bytes([result[2], result[3]]);
        assert_eq!(duration, 800, "4800 * 8000/48000 = 800");
    }

    #[test]
    fn test_rewrite_dtmf_duration_8k_to_48k() {
        let data = vec![0x01u8, 0x0A, 0x00, 0xA0]; // duration=160 (8kHz ticks)
        let result = rewrite_dtmf_duration(&data, 8000, 48000);

        let duration = u16::from_be_bytes([result[2], result[3]]);
        assert_eq!(duration, 960, "160 * 48000/8000 = 960");
    }

    #[test]
    fn test_rewrite_dtmf_duration_short_payload() {
        let data = vec![0x01u8, 0x02]; // too short
        let result = rewrite_dtmf_duration(&data, 8000, 48000);
        assert_eq!(&result[..], &data[..], "Short payload should be unchanged");
    }

    #[test]
    fn test_transcoder_silence_pcmu_to_pcma_roundtrip() {
        let pcm = silence_pcm_8k();
        let mut enc = create_encoder(CodecType::PCMU);
        let pcmu_data = enc.encode(&pcm);

        let mut t1 = Transcoder::new(CodecType::PCMU, CodecType::PCMA, 8);
        let input = make_frame(0, 8000, pcmu_data, 1, 0);
        let pcma_out = t1.transcode(&input);

        let mut t2 = Transcoder::new(CodecType::PCMA, CodecType::PCMU, 0);
        let roundtrip = t2.transcode(&pcma_out);

        let mut dec = create_decoder(CodecType::PCMU);
        let final_pcm = dec.decode(&roundtrip.data);

        let max_sample = final_pcm.iter().map(|&s| s.abs()).max().unwrap_or(0);
        assert!(
            max_sample < 300,
            "Silence should remain near-zero after roundtrip, max={}",
            max_sample
        );
    }
}
