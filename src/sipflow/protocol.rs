use bytes::{Buf, BufMut, Bytes};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const PACKET_VERSION: u8 = 2;

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
    if magic != 0x5346 {
        return Err(anyhow::anyhow!("Invalid magic header"));
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
    buf.put_u16(0x5346); // Magic
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
        let mut metadata_buf = Vec::with_capacity(8 + call_id.len());
        metadata_buf.put_i32(packet.leg.unwrap_or(0));
        metadata_buf.put_u32(call_id.len() as u32);
        metadata_buf.extend_from_slice(call_id.as_bytes());
        buf.put_u32(metadata_buf.len() as u32);
        buf.extend_from_slice(&metadata_buf);
    } else {
        buf.put_u32(0);
    }

    buf.put_u32(packet.payload.len() as u32);
    buf.extend_from_slice(&packet.payload);

    buf
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
}
