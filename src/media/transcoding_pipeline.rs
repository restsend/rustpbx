use audio_codec::CodecType;

pub struct TranscodingPipeline {
    _input_codec: CodecType,
    _output_codec: CodecType,
}

const BIAS: i16 = 0x84;
const CLIP: i16 = 32635;

static PCMU_ENCODE_TABLE: [i16; 256] = {
    let mut table = [0i16; 256];
    let mut i: i16 = 0;
    while i < 256 {
        let val = (i as i16) - 128;
        let sign = if val < 0 { 0x00 } else { 0x80 };
        let mut magnitude = if val < 0 { -val } else { val };
        let mut chord = 7;
        let step: i16;
        if magnitude > 32766 {
            magnitude = 32766;
        }
        magnitude += BIAS;
        if magnitude > CLIP {
            magnitude = CLIP;
        }
        let mut mag = magnitude as i16;
        while mag > 15 {
            mag >>= 1;
            chord -= 1;
            if chord < 0 {
                chord = 0;
                break;
            }
        }
        step = ((chord as i16) << 4) | (mag - 15) as i16;
        table[i as usize] = (sign | step as u8 as i16) ^ 0xFF;
        i += 1;
    }
    table
};

static PCMU_DECODE_TABLE: [i16; 256] = {
    let mut table = [0i16; 256];
    let mut i: i16 = 0;
    while i < 256 {
        let val = (i as u8) ^ 0xFF;
        let sign = if (val & 0x80) != 0 { 1i16 } else { -1i16 };
        let chord = ((val & 0x70) >> 4) as i16;
        let step = (val & 0x0F) as i16;
        let mut magnitude = ((step << 1) + 33) << chord;
        magnitude -= 33;
        table[i as usize] = sign * magnitude as i16;
        i += 1;
    }
    table
};

impl TranscodingPipeline {
    pub fn new(input_codec: CodecType, output_codec: CodecType) -> Self {
        Self {
            _input_codec: input_codec,
            _output_codec: output_codec,
        }
    }

    pub fn encode_from_pcm(&self, pcm: &[i16]) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(pcm.len());
        for &sample in pcm {
            let s = sample.clamp(-32768, 32767);
            let idx = ((s >> 8) + 128) as usize;
            encoded.push(PCMU_ENCODE_TABLE[idx] as u8);
        }
        encoded
    }

    pub fn decode_to_pcm(&self, encoded: &[u8]) -> Vec<i16> {
        let mut pcm = Vec::with_capacity(encoded.len());
        for &byte in encoded {
            pcm.push(PCMU_DECODE_TABLE[byte as usize]);
        }
        pcm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pcmu_roundtrip() {
        let pipeline = TranscodingPipeline::new(CodecType::PCMU, CodecType::PCMU);
        let pcm: Vec<i16> = (0..160).map(|i| (i as i16 * 100) % 32767).collect();
        let encoded = pipeline.encode_from_pcm(&pcm);
        assert_eq!(encoded.len(), 160);
        let decoded = pipeline.decode_to_pcm(&encoded);
        assert_eq!(decoded.len(), 160);
    }
}
