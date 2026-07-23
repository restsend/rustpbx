use bytes::{Buf, BufMut, Bytes};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const PACKET_VERSION: u8 = 2;

/// Magic header for a single-packet datagram ("SF").
pub const PACKET_MAGIC: u16 = 0x5346;

/// Magic header for a batched datagram ("SG" = Sipflow Group).
///
/// A batch datagram carries up to [`MAX_BATCH_COUNT`] packets in a single
/// UDP datagram to amortize syscall and allocation overhead on the sender
/// side. The receiver dispatches on the magic to decide whether to parse a
/// single packet or a batch.
pub const BATCH_MAGIC: u16 = 0x5347;
const BATCH_VERSION: u8 = 1;

/// Maximum number of packets that can be packed into a single batch
/// datagram. Limited by the `u16` count field on the wire.
pub const MAX_BATCH_COUNT: usize = u16::MAX as usize;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MsgType {
    Sip = 0,
    Rtp = 1,
}

#[derive(Debug)]
pub struct Packet {
    pub msg_type: MsgType,
    pub src: (IpAddr, u16),
    pub dst: (IpAddr, u16),
    pub timestamp: u64,
    pub call_id: Option<String>,
    pub leg: Option<i32>,
    pub payload: Bytes,
}

pub fn parse_packet(data: &[u8]) -> anyhow::Result<Packet> {
    let mut buf = data;
    let magic = buf.try_get_u16()?;
    if magic != PACKET_MAGIC {
        return Err(anyhow::anyhow!(
            "Invalid magic header: expected 0x{:04X}, got 0x{:04X}",
            PACKET_MAGIC,
            magic
        ));
    }
    let version = buf.try_get_u8()?;
    if version != PACKET_VERSION {
        return Err(anyhow::anyhow!("Unsupported packet version"));
    }
    let msg_type_raw = buf.try_get_u8()?;
    let msg_type = match msg_type_raw {
        0 => MsgType::Sip,
        1 => MsgType::Rtp,
        _ => return Err(anyhow::anyhow!("Invalid msg type")),
    };
    let ip_family = buf.try_get_u8()?;
    let src_ip = match ip_family {
        4 => IpAddr::V4(Ipv4Addr::from(buf.try_get_u32()?)),
        6 => IpAddr::V6(Ipv6Addr::from(buf.try_get_u128()?)),
        _ => return Err(anyhow::anyhow!("Invalid IP family")),
    };
    let src_port = buf.try_get_u16()?;
    let dst_ip = match ip_family {
        4 => IpAddr::V4(Ipv4Addr::from(buf.try_get_u32()?)),
        6 => IpAddr::V6(Ipv6Addr::from(buf.try_get_u128()?)),
        _ => return Err(anyhow::anyhow!("Invalid IP family")),
    };
    let dst_port = buf.try_get_u16()?;
    let timestamp = buf.try_get_u64()?;
    let metadata_len = buf.try_get_u32()? as usize;
    let (call_id, leg) = if metadata_len == 0 {
        (None, None)
    } else {
        if buf.remaining() < metadata_len {
            return Err(anyhow::anyhow!("RTP metadata length exceeds packet size"));
        }

        let mut metadata = &buf[..metadata_len];
        buf.advance(metadata_len);

        let leg = metadata.try_get_i32()?;
        let call_id_len = metadata.try_get_u32()? as usize;
        if metadata.remaining() < call_id_len {
            return Err(anyhow::anyhow!("RTP call id length exceeds metadata size"));
        }

        let mut call_id = vec![0u8; call_id_len];
        metadata.try_copy_to_slice(&mut call_id)?;

        if metadata.has_remaining() {
            return Err(anyhow::anyhow!("Trailing bytes in RTP metadata"));
        }

        (Some(String::from_utf8(call_id)?), Some(leg))
    };
    let payload_len = buf.try_get_u32()? as usize;
    if buf.remaining() < payload_len {
        return Err(anyhow::anyhow!("Payload length exceeds packet size"));
    }
    let payload = Bytes::copy_from_slice(&buf[..payload_len]);

    Ok(Packet {
        msg_type,
        src: (src_ip, src_port),
        dst: (dst_ip, dst_port),
        timestamp,
        call_id,
        leg,
        payload: payload.into(),
    })
}

/// Encode a packet for transmission
pub fn encode_packet(packet: &Packet) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_packet_into(&mut buf, packet);
    buf
}

/// Encode a packet by appending it to an existing buffer.
///
/// This is the zero-allocation building block used by both [`encode_packet`]
/// and [`encode_batch_into`]. The caller is responsible for ensuring `buf`
/// has enough capacity (or accepting the amortized growth cost).
pub fn encode_packet_into(buf: &mut Vec<u8>, packet: &Packet) {
    buf.put_u16(PACKET_MAGIC); // Magic
    buf.put_u8(PACKET_VERSION);
    buf.put_u8(packet.msg_type as u8);

    let ip_family = match packet.src.0 {
        IpAddr::V4(_) => 4u8,
        IpAddr::V6(_) => 6u8,
    };
    buf.put_u8(ip_family);

    // Source IP
    match packet.src.0 {
        IpAddr::V4(ip) => buf.put_u32(u32::from(ip)),
        IpAddr::V6(ip) => buf.put_u128(u128::from(ip)),
    }
    buf.put_u16(packet.src.1);

    // Dest IP
    match packet.dst.0 {
        IpAddr::V4(ip) => buf.put_u32(u32::from(ip)),
        IpAddr::V6(ip) => buf.put_u128(u128::from(ip)),
    }
    buf.put_u16(packet.dst.1);

    buf.put_u64(packet.timestamp);

    if packet.call_id.is_some() || packet.leg.is_some() {
        let call_id = packet.call_id.as_deref().unwrap_or("");
        // Inline metadata construction directly into `buf` to avoid the
        // intermediate Vec allocation the previous version performed.
        let metadata_len_pos = buf.len();
        buf.put_u32(0); // metadata length placeholder
        let metadata_start = buf.len();
        buf.put_i32(packet.leg.unwrap_or(0));
        buf.put_u32(call_id.len() as u32);
        buf.extend_from_slice(call_id.as_bytes());
        let metadata_len = (buf.len() - metadata_start) as u32;
        // Backfill the metadata length field.
        buf[metadata_len_pos..metadata_len_pos + 4].copy_from_slice(&metadata_len.to_be_bytes());
    } else {
        buf.put_u32(0);
    }

    buf.put_u32(packet.payload.len() as u32);
    buf.extend_from_slice(&packet.payload);
}

/// Encode a batch of packets into a single datagram buffer.
///
/// Layout:
/// ```text
/// BATCH_MAGIC (u16) | BATCH_VERSION (u8) | count (u16)
///   | frame_len_0 (u32) | frame_0
///   | frame_len_1 (u32) | frame_1
///   | ...
/// ```
/// Each `frame_i` is exactly what [`encode_packet_into`] would produce for
/// that packet (i.e. it begins with `PACKET_MAGIC`). The redundant
/// per-frame length prefix allows robust parsing and skipping.
///
/// Returns an error if `packets.len() > MAX_BATCH_COUNT`.
pub fn encode_batch(packets: &[Packet]) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    encode_batch_into(&mut buf, packets)?;
    Ok(buf)
}

/// Like [`encode_batch`] but appends to a caller-provided buffer.
///
/// The buffer is NOT cleared; callers can `clear()` a reusable buffer
/// before each call to keep allocations steady-state across batches.
pub fn encode_batch_into(buf: &mut Vec<u8>, packets: &[Packet]) -> anyhow::Result<()> {
    if packets.len() > MAX_BATCH_COUNT {
        anyhow::bail!(
            "batch too large: {} > MAX_BATCH_COUNT ({})",
            packets.len(),
            MAX_BATCH_COUNT
        );
    }
    buf.put_u16(BATCH_MAGIC);
    buf.put_u8(BATCH_VERSION);
    buf.put_u16(packets.len() as u16);
    for p in packets {
        let len_pos = buf.len();
        buf.put_u32(0); // frame length placeholder
        let start = buf.len();
        encode_packet_into(buf, p);
        let frame_len = (buf.len() - start) as u32;
        buf[len_pos..len_pos + 4].copy_from_slice(&frame_len.to_be_bytes());
    }
    Ok(())
}

/// Parse a UDP datagram that may contain either a single packet (legacy
/// `PACKET_MAGIC`) or a batch (`BATCH_MAGIC`).
///
/// The returned `Vec` will contain exactly one packet for the single
/// format and `count` packets for the batch format. Callers that don't
/// need the Vec allocation on the single-packet path can call
/// [`parse_packet`] directly.
pub fn parse_datagram(data: &[u8]) -> anyhow::Result<Vec<Packet>> {
    if data.len() < 2 {
        return Err(anyhow::anyhow!("datagram too short for magic header"));
    }
    let magic = u16::from_be_bytes([data[0], data[1]]);
    match magic {
        PACKET_MAGIC => Ok(vec![parse_packet(data)?]),
        BATCH_MAGIC => parse_batch(data),
        other => Err(anyhow::anyhow!("Invalid datagram magic: 0x{:04X}", other)),
    }
}

fn parse_batch(data: &[u8]) -> anyhow::Result<Vec<Packet>> {
    let mut buf = data;
    let magic = buf.try_get_u16()?;
    debug_assert_eq!(magic, BATCH_MAGIC);
    let version = buf.try_get_u8()?;
    if version != BATCH_VERSION {
        return Err(anyhow::anyhow!(
            "Unsupported batch version: {} (expected {})",
            version,
            BATCH_VERSION
        ));
    }
    let count = buf.try_get_u16()? as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let frame_len = buf.try_get_u32()? as usize;
        if buf.remaining() < frame_len {
            return Err(anyhow::anyhow!(
                "batch frame {i}: declared length {frame_len} exceeds remaining {} bytes",
                buf.remaining()
            ));
        }
        let frame = &buf[..frame_len];
        buf.advance(frame_len);
        out.push(parse_packet(frame)?);
    }
    if buf.has_remaining() {
        return Err(anyhow::anyhow!(
            "trailing {} bytes after {count} batch frames",
            buf.remaining()
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_rtp_metadata_roundtrip() {
        let rtp = Bytes::from_static(b"\x80\x00\x00\x2a\x00\x00\x00\xa0\x00\x00\x00\x01payload");
        let packet = Packet {
            msg_type: MsgType::Rtp,
            src: (IpAddr::from([127, 0, 0, 1]), 0),
            dst: (IpAddr::from([127, 0, 0, 1]), 0),
            timestamp: 123,
            call_id: Some("call-123".to_string()),
            leg: Some(0),
            payload: rtp.clone(),
        };

        let decoded = parse_packet(&encode_packet(&packet)).expect("packet should decode");

        assert_eq!(decoded.msg_type, MsgType::Rtp);
        assert_eq!(decoded.payload, rtp);
        assert_eq!(decoded.call_id.as_deref(), Some("call-123"));
        assert_eq!(decoded.leg, Some(0));
    }

    #[test]
    fn test_packet_wire_format_uses_big_endian() {
        let packet = Packet {
            msg_type: MsgType::Rtp,
            src: (IpAddr::from([1, 2, 3, 4]), 0x1234),
            dst: (IpAddr::from([5, 6, 7, 8]), 0x5678),
            timestamp: 0x0102_0304_0506_0708,
            call_id: Some("abc".to_string()),
            leg: Some(0x0102_0304),
            payload: Bytes::from_static(&[0xaa, 0xbb]),
        };

        assert_eq!(
            encode_packet(&packet),
            vec![
                0x53, 0x46, // magic
                0x02, // wrapper version
                0x01, // RTP
                0x04, // IPv4
                0x01, 0x02, 0x03, 0x04, // src IP
                0x12, 0x34, // src port
                0x05, 0x06, 0x07, 0x08, // dst IP
                0x56, 0x78, // dst port
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // timestamp
                0x00, 0x00, 0x00, 0x0b, // metadata length
                0x01, 0x02, 0x03, 0x04, // leg
                0x00, 0x00, 0x00, 0x03, // call id length
                b'a', b'b', b'c', // call id
                0x00, 0x00, 0x00, 0x02, // payload length
                0xaa, 0xbb, // payload
            ]
        );
    }

    #[test]
    fn test_packet_ipv6_wire_format_uses_big_endian() {
        let packet = Packet {
            msg_type: MsgType::Sip,
            src: (
                IpAddr::V6(Ipv6Addr::new(
                    0x0102, 0x0304, 0x0506, 0x0708, 0x090a, 0x0b0c, 0x0d0e, 0x0f10,
                )),
                0x1234,
            ),
            dst: (
                IpAddr::V6(Ipv6Addr::new(
                    0x1112, 0x1314, 0x1516, 0x1718, 0x191a, 0x1b1c, 0x1d1e, 0x1f20,
                )),
                0x5678,
            ),
            timestamp: 0x0102_0304_0506_0708,
            call_id: None,
            leg: None,
            payload: Bytes::from_static(&[0xaa]),
        };

        assert_eq!(
            encode_packet(&packet),
            vec![
                0x53, 0x46, // magic
                0x02, // wrapper version
                0x00, // SIP
                0x06, // IPv6
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // src IP
                0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, // src IP
                0x12, 0x34, // src port
                0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, // dst IP
                0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, // dst IP
                0x56, 0x78, // dst port
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // timestamp
                0x00, 0x00, 0x00, 0x00, // metadata length
                0x00, 0x00, 0x00, 0x01, // payload length
                0xaa, // payload
            ]
        );
    }

    #[test]
    fn test_parse_packet_rejects_unsupported_version() {
        let packet = Packet {
            msg_type: MsgType::Sip,
            src: (IpAddr::from([127, 0, 0, 1]), 5060),
            dst: (IpAddr::from([127, 0, 0, 1]), 5060),
            timestamp: 123,
            call_id: None,
            leg: None,
            payload: Bytes::from_static(b"INVITE sip:bob@example.com SIP/2.0\r\n\r\n"),
        };
        let mut data = encode_packet(&packet);
        data[2] = PACKET_VERSION + 1;

        let err = parse_packet(&data).expect_err("unsupported version should fail");

        assert!(err.to_string().contains("Unsupported packet version"));
    }

    fn sample_packet(idx: u8) -> Packet {
        Packet {
            msg_type: if idx.is_multiple_of(2) {
                MsgType::Rtp
            } else {
                MsgType::Sip
            },
            src: (IpAddr::from([10, 0, 0, idx]), 1000 + idx as u16),
            dst: (IpAddr::from([192, 168, 0, idx]), 2000 + idx as u16),
            timestamp: 1_000_000 + idx as u64,
            call_id: Some(format!("call-{}", idx)),
            leg: Some(idx as i32),
            payload: Bytes::from(vec![idx; idx as usize + 1]),
        }
    }

    #[test]
    fn test_batch_roundtrip_multiple_packets() {
        let packets: Vec<Packet> = (0..5).map(sample_packet).collect();

        let encoded = encode_batch(&packets).expect("encode should succeed");
        let decoded = parse_datagram(&encoded).expect("datagram should parse");

        assert_eq!(decoded.len(), packets.len());
        for (i, (orig, got)) in packets.iter().zip(decoded.iter()).enumerate() {
            assert_eq!(got.msg_type, orig.msg_type, "msg_type mismatch at {i}");
            assert_eq!(got.src, orig.src, "src mismatch at {i}");
            assert_eq!(got.dst, orig.dst, "dst mismatch at {i}");
            assert_eq!(got.timestamp, orig.timestamp, "ts mismatch at {i}");
            assert_eq!(got.call_id, orig.call_id, "call_id mismatch at {i}");
            assert_eq!(got.leg, orig.leg, "leg mismatch at {i}");
            assert_eq!(got.payload, orig.payload, "payload mismatch at {i}");
        }
    }

    #[test]
    fn test_batch_single_packet_roundtrip() {
        let packets = vec![sample_packet(7)];
        let encoded = encode_batch(&packets).expect("encode");
        let decoded = parse_datagram(&encoded).expect("parse");
        assert_eq!(decoded.len(), 1);
    }

    #[test]
    fn test_batch_max_count_allowed() {
        // Exactly MAX_BATCH_COUNT should be accepted.
        let packets: Vec<Packet> = (0..MAX_BATCH_COUNT)
            .map(|i| (i % 256) as u8)
            .map(sample_packet)
            .collect();
        let encoded = encode_batch(&packets).expect("encode at the boundary should succeed");
        let decoded = parse_datagram(&encoded).expect("parse at boundary");
        assert_eq!(decoded.len(), MAX_BATCH_COUNT);
    }

    #[test]
    fn test_batch_too_large_rejected() {
        let packets: Vec<Packet> = (0..=MAX_BATCH_COUNT)
            .map(|i| (i % 256) as u8)
            .map(sample_packet)
            .collect();
        let err = encode_batch(&packets).expect_err("should reject oversized batch");
        assert!(
            err.to_string().contains("batch too large"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_encode_batch_into_appends_without_clearing() {
        let packets_a = vec![sample_packet(1)];
        let packets_b = vec![sample_packet(2), sample_packet(3)];

        let mut buf = Vec::new();
        encode_batch_into(&mut buf, &packets_a).unwrap();
        let first_len = buf.len();
        // Second call should APPEND (not overwrite), producing two batches in
        // one buffer. The caller is responsible for clearing between calls.
        encode_batch_into(&mut buf, &packets_b).unwrap();
        assert!(buf.len() > first_len);

        // Each batch is independently parseable from its slice.
        let parsed_a = parse_batch(&buf[..first_len]).expect("first batch");
        assert_eq!(parsed_a.len(), 1);
    }

    #[test]
    fn test_parse_datagram_dispatches_single_packet() {
        // Single-packet datagrams (legacy `PACKET_MAGIC`) must still parse
        // through `parse_datagram` for backward compatibility.
        let packet = sample_packet(0);
        let encoded = encode_packet(&packet);
        let decoded = parse_datagram(&encoded).expect("single-packet path");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].timestamp, packet.timestamp);
    }

    #[test]
    fn test_parse_datagram_rejects_unknown_magic() {
        let data = [0xAB, 0xCD, 0x00, 0x00];
        let err = parse_datagram(&data).expect_err("unknown magic");
        assert!(
            err.to_string().contains("Invalid datagram magic"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_datagram_rejects_short_input() {
        let err = parse_datagram(&[0x53]).expect_err("too short");
        assert!(err.to_string().contains("datagram too short"));
    }

    #[test]
    fn test_parse_batch_rejects_bad_version() {
        let mut encoded = encode_batch(&[sample_packet(0)]).expect("encode");
        // BATCH_VERSION sits at byte offset 2 (after the 2-byte magic).
        encoded[2] = BATCH_VERSION + 1;
        let err = parse_datagram(&encoded).expect_err("bad batch version");
        assert!(err.to_string().contains("Unsupported batch version"));
    }

    #[test]
    fn test_parse_batch_rejects_truncated_frame() {
        let mut encoded = encode_batch(&[sample_packet(0), sample_packet(1)]).expect("encode");
        // Truncate the tail so the last frame's declared length no longer fits.
        encoded.truncate(encoded.len() - 1);
        let err = parse_datagram(&encoded).expect_err("truncated frame");
        assert!(
            err.to_string().contains("exceeds remaining"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_batch_rejects_trailing_bytes() {
        let mut encoded = encode_batch(&[sample_packet(0)]).expect("encode");
        encoded.push(0xFF); // dangling byte after the declared frames
        let err = parse_datagram(&encoded).expect_err("trailing bytes");
        assert!(err.to_string().contains("trailing"));
    }

    #[test]
    fn test_encode_packet_into_matches_encode_packet() {
        let packet = sample_packet(9);
        let mut buf = Vec::new();
        encode_packet_into(&mut buf, &packet);
        assert_eq!(buf, encode_packet(&packet));
    }

    #[test]
    fn test_batch_wire_format_header() {
        // Header: magic(2) + version(1) + count(2) + frame_len(4) + frame_body...
        let packets = vec![sample_packet(0)];
        let encoded = encode_batch(&packets).unwrap();
        // Magic
        assert_eq!(&encoded[0..2], &[0x53, 0x47]);
        // Version
        assert_eq!(encoded[2], BATCH_VERSION);
        // Count
        assert_eq!(u16::from_be_bytes([encoded[3], encoded[4]]), 1);
        // First frame length prefix
        let frame_len =
            u32::from_be_bytes([encoded[5], encoded[6], encoded[7], encoded[8]]) as usize;
        // The frame body must be exactly the bytes that encode_packet produces.
        let expected_frame = encode_packet(&packets[0]);
        assert_eq!(frame_len, expected_frame.len());
        assert_eq!(&encoded[9..9 + frame_len], &expected_frame[..]);
    }
}
