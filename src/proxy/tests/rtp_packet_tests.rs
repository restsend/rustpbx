use super::rtp_utils::RtpPacket;
use anyhow::Result;
use tracing::info;

#[tokio::test]
async fn test_rtp_packet_integrity() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let test_ssrc = 0xDEADBEEFu32;
    let test_seq_start = 1000u16;
    let packets = RtpPacket::create_sequence(50, test_seq_start, 50000, test_ssrc, 0, 160, 160);

    for (i, packet) in packets.iter().enumerate() {
        let encoded = packet.encode();
        let decoded = RtpPacket::decode(&encoded)?;

        assert_eq!(decoded.version, 2, "RTP version should be 2");
        assert_eq!(decoded.payload_type, 0, "Payload type should be 0 (PCMU)");
        assert_eq!(
            decoded.sequence_number,
            test_seq_start + i as u16,
            "Sequence number mismatch"
        );
        assert_eq!(decoded.ssrc, test_ssrc, "SSRC mismatch");
        assert_eq!(
            decoded.timestamp,
            50000 + (i as u32) * 160,
            "Timestamp mismatch"
        );
        assert_eq!(decoded.payload, packet.payload, "Payload mismatch");
    }

    info!("RTP packet integrity test passed");
    Ok(())
}

#[tokio::test]
async fn test_rtp_sequence_validation() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let packets = RtpPacket::create_sequence(100, 5000, 100000, 0x12345678, 0, 160, 160);

    let mut last_seq: Option<u16> = None;
    let mut last_ts: Option<u32> = None;

    for packet in &packets {
        if let Some(last) = last_seq {
            let expected = last.wrapping_add(1);
            assert_eq!(
                packet.sequence_number, expected,
                "Sequence gap detected: expected {}, got {}",
                expected, packet.sequence_number
            );
        }
        last_seq = Some(packet.sequence_number);

        if let Some(last) = last_ts {
            let expected = last + 160;
            assert_eq!(
                packet.timestamp, expected,
                "Timestamp jump detected: expected {}, got {}",
                expected, packet.timestamp
            );
        }
        last_ts = Some(packet.timestamp);

        assert_eq!(packet.ssrc, 0x12345678, "SSRC should be constant");
    }

    info!("RTP sequence validation test passed");
    Ok(())
}

#[tokio::test]
async fn test_rtp_various_payload_sizes() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    for payload_size in [80, 160, 240, 320] {
        let packets = RtpPacket::create_sequence(
            10,
            1000,
            50000,
            0x12345678,
            0,
            payload_size,
            payload_size as u32,
        );

        assert_eq!(packets.len(), 10);

        for packet in &packets {
            assert_eq!(packet.payload.len(), payload_size);

            let encoded = packet.encode();
            let decoded = RtpPacket::decode(&encoded)?;
            assert_eq!(decoded.payload.len(), payload_size);
        }

        info!(payload_size, "Payload size test passed");
    }

    Ok(())
}

#[tokio::test]
async fn test_rtp_different_codecs() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let codecs = vec![(0, "PCMU", 160), (8, "PCMA", 160), (18, "G729", 20)];

    for (pt, name, frame_size) in codecs {
        let packets = RtpPacket::create_sequence(
            10,
            1000,
            50000,
            0x12345678,
            pt,
            frame_size,
            frame_size as u32,
        );

        for packet in &packets {
            assert_eq!(
                packet.payload_type, pt,
                "Payload type mismatch for {}",
                name
            );

            let encoded = packet.encode();
            let decoded = RtpPacket::decode(&encoded)?;
            assert_eq!(
                decoded.payload_type, pt,
                "Payload type not preserved for {}",
                name
            );
        }

        info!(codec = name, payload_type = pt, "Codec test passed");
    }

    Ok(())
}

#[tokio::test]
async fn test_rtp_data_integrity() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let test_ssrc = 0x12345678u32;
    let test_pt = 0u8;

    let packets = RtpPacket::create_sequence(100, 1000, 50000, test_ssrc, test_pt, 160, 160);

    assert_eq!(packets.len(), 100);

    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(packet.ssrc, test_ssrc);
        assert_eq!(packet.payload_type, test_pt);
        assert_eq!(packet.sequence_number, 1000 + i as u16);
        assert_eq!(packet.timestamp, 50000 + (i as u32) * 160);
    }

    for packet in &packets {
        let encoded = packet.encode();
        let decoded = RtpPacket::decode(&encoded).unwrap();
        assert_eq!(decoded.ssrc, packet.ssrc);
        assert_eq!(decoded.sequence_number, packet.sequence_number);
        assert_eq!(decoded.timestamp, packet.timestamp);
    }

    info!("RTP data integrity test completed");
    Ok(())
}
