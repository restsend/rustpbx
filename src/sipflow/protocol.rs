use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use bytes::Bytes;
use std::io::Cursor;
use std::net::IpAddr;

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
    pub payload: Bytes,
}

pub fn parse_packet(data: &[u8]) -> anyhow::Result<Packet> {
    let mut cursor = Cursor::new(data);
    let magic = cursor.read_u16::<BigEndian>()?;
    if magic != 0x5346 {
        return Err(anyhow::anyhow!("Invalid magic header"));
    }
    let _version = cursor.read_u8()?;
    let msg_type_raw = cursor.read_u8()?;
    let msg_type = match msg_type_raw {
        0 => MsgType::Sip,
        1 => MsgType::Rtp,
        _ => return Err(anyhow::anyhow!("Invalid msg type")),
    };
    let ip_family = cursor.read_u8()?;
    let src_ip = if ip_family == 4 {
        let mut addr = [0u8; 4];
        std::io::Read::read_exact(&mut cursor, &mut addr)?;
        IpAddr::from(addr)
    } else {
        let mut addr = [0u8; 16];
        std::io::Read::read_exact(&mut cursor, &mut addr)?;
        IpAddr::from(addr)
    };
    let src_port = cursor.read_u16::<BigEndian>()?;
    let dst_ip = if ip_family == 4 {
        let mut addr = [0u8; 4];
        std::io::Read::read_exact(&mut cursor, &mut addr)?;
        IpAddr::from(addr)
    } else {
        let mut addr = [0u8; 16];
        std::io::Read::read_exact(&mut cursor, &mut addr)?;
        IpAddr::from(addr)
    };
    let dst_port = cursor.read_u16::<BigEndian>()?;
    let timestamp = cursor.read_u64::<BigEndian>()?;
    let payload_len = cursor.read_u32::<BigEndian>()?;
    let mut payload = vec![0u8; payload_len as usize];
    std::io::Read::read_exact(&mut cursor, &mut payload)?;

    Ok(Packet {
        msg_type,
        src: (src_ip, src_port),
        dst: (dst_ip, dst_port),
        timestamp,
        payload: payload.into(),
    })
}

/// Encode a packet for transmission
pub fn encode_packet(packet: &Packet) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.write_u16::<BigEndian>(0x5346).unwrap(); // Magic
    buf.write_u8(1).unwrap(); // Version
    buf.write_u8(packet.msg_type as u8).unwrap();

    let ip_family = match packet.src.0 {
        IpAddr::V4(_) => 4u8,
        IpAddr::V6(_) => 6u8,
    };
    buf.write_u8(ip_family).unwrap();

    // Source IP
    match packet.src.0 {
        IpAddr::V4(ip) => buf.extend_from_slice(&ip.octets()),
        IpAddr::V6(ip) => buf.extend_from_slice(&ip.octets()),
    }
    buf.write_u16::<BigEndian>(packet.src.1).unwrap();

    // Dest IP
    match packet.dst.0 {
        IpAddr::V4(ip) => buf.extend_from_slice(&ip.octets()),
        IpAddr::V6(ip) => buf.extend_from_slice(&ip.octets()),
    }
    buf.write_u16::<BigEndian>(packet.dst.1).unwrap();

    buf.write_u64::<BigEndian>(packet.timestamp).unwrap();
    buf.write_u32::<BigEndian>(packet.payload.len() as u32)
        .unwrap();
    buf.extend_from_slice(&packet.payload);

    buf
}
