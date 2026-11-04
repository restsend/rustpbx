//! Binary data-plane framing (UDP relay datagrams).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use tokio::sync::mpsc;

pub const HEADER_LEN: usize = 7; // u32 total_len + u8 kind + u16 listener_id

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    UdpUp = 1,
    UdpDown = 2,
    /// Worker → master: SIP message to deliver over a REAL connection owned
    /// by another generation. Routed via ConnTable[(kind, peer)].
    /// `listener_id` low byte = ConnKind, `addr` = target peer.
    RelayToConn = 3,
    /// Worker → master: SIP message read from a REAL connection whose dialog
    /// is owned by another generation. Routed via DialogTable[call_id in
    /// data]. `listener_id` low byte = ConnKind, `addr` = originating peer.
    RelayToDialog = 4,
    /// Master → worker: a relayed SIP message to inject locally. If the
    /// worker holds the REAL connection → inject into its write path;
    /// otherwise (dialog owner) → inject as inbound on a (lazily created)
    /// virtual relay connection. Same encoding as RelayToConn.
    RelayDeliver = 5,
}

impl FrameKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(FrameKind::UdpUp),
            2 => Some(FrameKind::UdpDown),
            3 => Some(FrameKind::RelayToConn),
            4 => Some(FrameKind::RelayToDialog),
            5 => Some(FrameKind::RelayDeliver),
            _ => None,
        }
    }
}

/// Pack/unpack a `ConnKind` into the frame's `listener_id` field (low byte;
/// UDP relay frames use the same field for the listener id, relay frames for
/// the connection class).
pub fn unpack_conn_kind(id: u16) -> Option<crate::control::ConnKind> {
    crate::control::ConnKind::from_u8((id & 0xff) as u8)
}

pub fn put_addr(buf: &mut Vec<u8>, addr: &SocketAddr) {
    match addr {
        SocketAddr::V4(a) => {
            buf.push(4);
            buf.extend_from_slice(&a.ip().octets());
            buf.extend_from_slice(&a.port().to_be_bytes());
        }
        SocketAddr::V6(a) => {
            buf.push(6);
            buf.extend_from_slice(&a.ip().octets());
            buf.extend_from_slice(&a.port().to_be_bytes());
        }
    }
}

pub fn read_addr(buf: &[u8]) -> Option<(SocketAddr, usize)> {
    if buf.is_empty() {
        return None;
    }
    match buf[0] {
        4 => {
            if buf.len() < 7 { return None; }
            let ip = Ipv4Addr::new(buf[1], buf[2], buf[3], buf[4]);
            Some((SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be_bytes([buf[5], buf[6]]))), 7))
        }
        6 => {
            if buf.len() < 19 { return None; }
            let mut o = [0u8; 16];
            o.copy_from_slice(&buf[1..17]);
            Some((SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(o), u16::from_be_bytes([buf[17], buf[18]]), 0, 0)), 19))
        }
        _ => None,
    }
}

/// Encode one datagram frame: `u32 BE total_len | u8 kind | u16 id | addr | datagram`
pub fn encode_datagram(kind: FrameKind, id: u16, addr: &SocketAddr, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(3 + 19 + data.len());
    body.push(kind as u8);
    body.extend_from_slice(&id.to_be_bytes());
    put_addr(&mut body, addr);
    body.extend_from_slice(data);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

#[derive(Debug)]
pub struct DatagramFrame {
    pub kind: FrameKind,
    pub listener_id: u16,
    pub addr: SocketAddr,
    pub datagram: Vec<u8>,
}

pub struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    pub fn new() -> Self { Self { buf: Vec::new() } }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    pub fn next_frame(&mut self) -> Option<DatagramFrame> {
        loop {
            if self.buf.len() < HEADER_LEN { return None; }
            let total = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if total < HEADER_LEN { return None; }
            if self.buf.len() < total { return None; }
            // Rolling upgrades: a peer speaking a NEWER protocol sends frame
            // kinds this side doesn't know. Skip the whole frame (the length
            // prefix keeps the stream aligned) instead of jamming the parser
            // — an unknown kind must never wedge the UDP relay behind it.
            let Some(kind) = FrameKind::from_u8(self.buf[4]) else {
                self.buf.drain(..total);
                continue;
            };
            let listener_id = u16::from_be_bytes([self.buf[5], self.buf[6]]);
            let (addr, consumed) = read_addr(&self.buf[HEADER_LEN..])?;
            if self.buf.len() < HEADER_LEN + consumed { return None; }
            let datagram = self.buf[HEADER_LEN + consumed..total].to_vec();
            self.buf.drain(..total);
            return Some(DatagramFrame { kind, listener_id, addr, datagram });
        }
    }
}

/// Bounded writer that queues frames for flushing to the data socket.
#[derive(Clone)]
pub struct DataWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

impl DataWriter {
    pub fn new(capacity: usize) -> (DataWriter, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (DataWriter { tx }, rx)
    }

    pub fn send(&self, frame: Vec<u8>) -> bool {
        self.tx.try_send(frame).is_ok()
    }
}

/// Encode a datagram for testing
#[cfg(test)]
pub fn encode_test_datagram(kind: FrameKind, id: u16, addr: &SocketAddr, data: &[u8]) -> Vec<u8> {
    encode_datagram(kind, id, addr, data)
}

#[cfg(test)]
mod relay_frame_tests {
    use super::*;
    use crate::control::ConnKind;

    #[test]
    fn conn_kind_roundtrip() {
        for k in [ConnKind::SipTcp, ConnKind::SipTls, ConnKind::Ws, ConnKind::Wss] {
            assert_eq!(ConnKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(ConnKind::from_u8(0), None);
        assert_eq!(ConnKind::from_u8(5), None);
    }

    /// Relay frames reuse the UDP datagram encoding: listener_id low byte
    /// carries the ConnKind, addr the peer, payload the SIP bytes. The same
    /// FrameParser must decode all three kinds.
    #[test]
    fn relay_frame_roundtrip_through_parser() {
        let peer: SocketAddr = "203.0.113.7:54323".parse().unwrap();
        for (kind, id) in [
            (FrameKind::RelayToConn, ConnKind::Wss.as_u8() as u16),
            (FrameKind::RelayToDialog, ConnKind::SipTcp.as_u8() as u16),
            (FrameKind::RelayDeliver, ConnKind::Ws.as_u8() as u16),
        ] {
            let frame = encode_datagram(kind, id, &peer, b"BYE sip:x SIP/2.0\r\n\r\n");
            let mut parser = FrameParser::new();
            parser.push(&frame);
            let df = parser.next_frame().expect("decoded");
            assert_eq!(df.kind, kind);
            assert_eq!(unpack_conn_kind(df.listener_id), Some(ConnKind::from_u8(id as u8).unwrap()));
            assert_eq!(df.addr, peer);
            assert_eq!(df.datagram, b"BYE sip:x SIP/2.0\r\n\r\n");
        }
    }

    /// Rolling upgrades: an unknown frame kind from a newer peer must be
    /// SKIPPED whole (length-prefixed), never jam the parser — the UDP relay
    /// frames behind it must still flow.
    #[test]
    fn unknown_frame_kind_is_skipped_not_jammed() {
        let peer: SocketAddr = "203.0.113.9:6000".parse().unwrap();
        // Hand-build a frame with kind = 200 (unknown): u32 len | u8 kind
        // | u16 id | v4 addr (1+4+2) | payload.
        let body_len = 1 + 2 + 7 + 4;
        let mut unknown: Vec<u8> = Vec::new();
        unknown.extend_from_slice(&((4 + body_len) as u32).to_be_bytes());
        unknown.push(200u8);
        unknown.extend_from_slice(&1u16.to_be_bytes());
        put_addr(&mut unknown, &peer);
        unknown.extend_from_slice(b"junk");
        assert_eq!(unknown.len(), 4 + body_len);

        let mut parser = FrameParser::new();
        parser.push(&unknown);
        let good = encode_datagram(FrameKind::UdpUp, 2, &peer, b"REGISTER");
        parser.push(&good);

        let df = parser.next_frame().expect("unknown skipped, good frame decoded");
        assert_eq!(df.kind, FrameKind::UdpUp);
        assert_eq!(df.datagram, b"REGISTER");
        assert!(parser.next_frame().is_none());
    }
}
