//! Self-tests for `tests/common/rtp_utils.rs`.
//! Extracted from the module's embedded `#[cfg(test)] mod tests` so the
//! harness self-tests run ONCE here instead of in every aggregator
//! binary (they used to re-execute ~11x per `cargo test` run).


use crate::common::rtp_utils::*;

#[test]
fn test_rtp_packet_encode_decode() {
    let packet = RtpPacket::new(
        0, // PCMU
        12345,
        987654321,
        0x12345678,
        vec![0xAB, 0xCD, 0xEF],
    );

    let encoded = packet.encode();
    let decoded = RtpPacket::decode(&encoded).unwrap();

    assert_eq!(decoded.version, 2);
    assert_eq!(decoded.payload_type, 0);
    assert_eq!(decoded.sequence_number, 12345);
    assert_eq!(decoded.timestamp, 987654321);
    assert_eq!(decoded.ssrc, 0x12345678);
    assert_eq!(decoded.payload, vec![0xAB, 0xCD, 0xEF]);
}

#[test]
fn test_extract_media_endpoint() {
    let sdp = "v=0\r\n\
        o=- 123456 123456 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 192.168.1.100\r\n\
        t=0 0\r\n\
        m=audio 10000 RTP/AVP 0\r\n";

    let endpoint = extract_media_endpoint(sdp);
    assert_eq!(endpoint, Some("192.168.1.100:10000".parse().unwrap()));
}

#[test]
fn test_create_sequence() {
    let packets = RtpPacket::create_sequence(
        10, 1000, 50000, 0xABCDEF01, 0, // PCMU
        160, 160, // 20ms at 8kHz
    );

    assert_eq!(packets.len(), 10);
    assert_eq!(packets[0].sequence_number, 1000);
    assert_eq!(packets[9].sequence_number, 1009);
    assert_eq!(packets[0].timestamp, 50000);
    assert_eq!(packets[9].timestamp, 50000 + 9 * 160);
}
