use anyhow::{Result, bail};

pub const SIP_PREFIX: &str = "sip:";
pub const RTP_PREFIX: &str = "rtp:";

pub fn make_sip_key(call_id: &str, counter: u64) -> String {
    format!("sip:{}:{:020}", call_id, counter)
}

pub fn make_rtp_key(call_id: &str, leg: i32, counter: u64) -> String {
    format!("rtp:{}:{}:{:020}", call_id, leg, counter)
}

pub fn sip_call_prefix(call_id: &str) -> String {
    format!("sip:{}:", call_id)
}

pub fn rtp_call_prefix(call_id: &str) -> String {
    format!("rtp:{}:", call_id)
}

pub fn rtp_call_leg_prefix(call_id: &str, leg: i32) -> String {
    format!("rtp:{}:{}:", call_id, leg)
}

pub fn encode_sip_value(src: &str, dst: &str, payload: &[u8]) -> Vec<u8> {
    let src_bytes = src.as_bytes();
    let dst_bytes = dst.as_bytes();
    let mut buf = Vec::with_capacity(4 + src_bytes.len() + 2 + dst_bytes.len() + payload.len());
    buf.extend_from_slice(&(src_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(src_bytes);
    buf.extend_from_slice(&(dst_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(dst_bytes);
    buf.extend_from_slice(payload);
    buf
}

pub fn decode_sip_value(value: &[u8]) -> Result<(String, String, Vec<u8>)> {
    if value.len() < 2 {
        bail!("sip value too short for src_len");
    }
    let src_len = u16::from_be_bytes([value[0], value[1]]) as usize;
    if value.len() < 2 + src_len + 2 {
        bail!("sip value too short for src + dst_len");
    }
    let src = String::from_utf8_lossy(&value[2..2 + src_len]).into_owned();
    let dst_len_offset = 2 + src_len;
    let dst_len = u16::from_be_bytes([value[dst_len_offset], value[dst_len_offset + 1]]) as usize;
    let dst_start = dst_len_offset + 2;
    if value.len() < dst_start + dst_len {
        bail!("sip value too short for dst + payload");
    }
    let dst = String::from_utf8_lossy(&value[dst_start..dst_start + dst_len]).into_owned();
    let payload = value[dst_start + dst_len..].to_vec();
    Ok((src, dst, payload))
}

pub fn encode_rtp_value(leg: i32, src: &str, payload: &[u8]) -> Vec<u8> {
    let src_bytes = src.as_bytes();
    let mut buf = Vec::with_capacity(4 + 2 + src_bytes.len() + payload.len());
    buf.extend_from_slice(&leg.to_be_bytes());
    buf.extend_from_slice(&(src_bytes.len() as u16).to_be_bytes());
    buf.extend_from_slice(src_bytes);
    buf.extend_from_slice(payload);
    buf
}

pub fn decode_rtp_value(value: &[u8]) -> Result<(i32, String, Vec<u8>)> {
    if value.len() < 6 {
        bail!("rtp value too short for leg + src_len");
    }
    let leg = i32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    let src_len = u16::from_be_bytes([value[4], value[5]]) as usize;
    if value.len() < 6 + src_len {
        bail!("rtp value too short for src + payload");
    }
    let src = String::from_utf8_lossy(&value[6..6 + src_len]).into_owned();
    let payload = value[6 + src_len..].to_vec();
    Ok((leg, src, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sip_value_roundtrip() {
        let src = "192.168.1.1:5060";
        let dst = "10.0.0.1:5060";
        let payload = b"INVITE sip:bob@example.com SIP/2.0\r\n\r\n";
        let encoded = encode_sip_value(src, dst, payload);
        let (dec_src, dec_dst, dec_payload) = decode_sip_value(&encoded).unwrap();
        assert_eq!(dec_src, src);
        assert_eq!(dec_dst, dst);
        assert_eq!(dec_payload, payload);
    }

    #[test]
    fn test_rtp_value_roundtrip() {
        let leg = 1i32;
        let src = "192.168.1.1:5004";
        let payload = b"\x80\x00\x00\x2a\x00\x00\x00\xa0\x00\x00\x00\x01payload";
        let encoded = encode_rtp_value(leg, src, payload);
        let (dec_leg, dec_src, dec_payload) = decode_rtp_value(&encoded).unwrap();
        assert_eq!(dec_leg, leg);
        assert_eq!(dec_src, src);
        assert_eq!(dec_payload, payload);
    }

    #[test]
    fn test_key_uniqueness() {
        let k1 = make_sip_key("call-1", 0);
        let k2 = make_sip_key("call-1", 1);
        assert_ne!(k1, k2);
        assert!(k1 < k2);
    }

    #[test]
    fn test_prefix_correctness() {
        let sip_key = make_sip_key("call-1", 42);
        let prefix = sip_call_prefix("call-1");
        assert!(sip_key.starts_with(&prefix));

        let rtp_key = make_rtp_key("call-1", 0, 42);
        let prefix = rtp_call_prefix("call-1");
        assert!(rtp_key.starts_with(&prefix));

        let rtp_leg_key = make_rtp_key("call-1", 1, 42);
        let leg_prefix = rtp_call_leg_prefix("call-1", 1);
        assert!(rtp_leg_key.starts_with(&leg_prefix));

        let other_prefix = rtp_call_leg_prefix("call-1", 0);
        assert!(!rtp_leg_key.starts_with(&other_prefix));
    }

    #[test]
    fn test_decode_corrupt_sip_value() {
        assert!(decode_sip_value(&[]).is_err());
        assert!(decode_sip_value(&[0x00]).is_err());
    }

    #[test]
    fn test_decode_corrupt_rtp_value() {
        assert!(decode_rtp_value(&[]).is_err());
        assert!(decode_rtp_value(&[0x00, 0x01]).is_err());
    }
}
