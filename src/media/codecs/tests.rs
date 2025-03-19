use super::*;

#[test]
fn test_pcmu_codec() {
    let mut encoder = pcmu::PcmuEncoder::new();
    let mut decoder = pcmu::PcmuDecoder::new();

    // Test with a simple sine wave
    let samples: Vec<i16> = (0..160)
        .map(|i| ((i as f32 * 0.1).sin() * 32767.0) as i16)
        .collect();

    // Encode
    let encoded = encoder.encode(&samples).unwrap();

    // Decode
    let decoded = decoder.decode(&encoded).unwrap();

    // Compare original and decoded samples
    // Note: Due to lossy compression, we use a tolerance
    for (orig, dec) in samples.iter().zip(decoded.iter()) {
        assert!((orig - dec).abs() < 2000); // Allow some difference due to compression
    }
}

#[test]
fn test_pcma_codec() {
    let mut encoder = pcma::PcmaEncoder::new();
    let mut decoder = pcma::PcmaDecoder::new();

    // Test with a simple sine wave
    let samples: Vec<i16> = (0..160)
        .map(|i| ((i as f32 * 0.1).sin() * 32767.0) as i16)
        .collect();

    // Encode
    let encoded = encoder.encode(&samples).unwrap();

    // Decode
    let decoded = decoder.decode(&encoded).unwrap();

    // Compare original and decoded samples
    // Note: Due to lossy compression, we use a tolerance
    for (orig, dec) in samples.iter().zip(decoded.iter()) {
        assert!((orig - dec).abs() < 2000); // Allow some difference due to compression
    }
}

#[test]
fn test_g722_codec() {
    let mut encoder = g722::G722Encoder::new(64000);
    let mut decoder = g722::G722Decoder::new(64000);

    // Test with a simple sine wave at 16kHz
    let samples: Vec<i16> = (0..320)
        .map(|i| ((i as f32 * 0.1).sin() * 32767.0) as i16)
        .collect();

    // Encode
    let encoded = encoder.encode(&samples).unwrap();

    // Decode
    let decoded = decoder.decode(&encoded).unwrap();

    // Compare original and decoded samples
    // Note: Due to lossy compression, we use a tolerance
    for (orig, dec) in samples.iter().zip(decoded.iter()) {
        assert!((orig - dec).abs() < 4000); // G.722 is more lossy, so we use a larger tolerance
    }
}

#[test]
fn test_codec_factory() {
    // Test decoder factory
    let decoder = create_decoder(CodecType::PCMU).unwrap();
    assert_eq!(decoder.sample_rate(), 8000);
    assert_eq!(decoder.channels(), 1);

    let decoder = create_decoder(CodecType::PCMA).unwrap();
    assert_eq!(decoder.sample_rate(), 8000);
    assert_eq!(decoder.channels(), 1);

    let decoder = create_decoder(CodecType::G722).unwrap();
    assert_eq!(decoder.sample_rate(), 16000);
    assert_eq!(decoder.channels(), 1);

    // Test encoder factory
    let encoder = create_encoder(CodecType::PCMU).unwrap();
    assert_eq!(encoder.sample_rate(), 8000);
    assert_eq!(encoder.channels(), 1);

    let encoder = create_encoder(CodecType::PCMA).unwrap();
    assert_eq!(encoder.sample_rate(), 8000);
    assert_eq!(encoder.channels(), 1);

    let encoder = create_encoder(CodecType::G722).unwrap();
    assert_eq!(encoder.sample_rate(), 16000);
    assert_eq!(encoder.channels(), 1);
}
