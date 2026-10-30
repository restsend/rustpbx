//! Binary data-plane framing (UDP relay datagrams).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use tokio::sync::mpsc;

pub const HEADER_LEN: usize = 7; // u32 total_len + u8 kind + u16 listener_id

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    UdpUp = 1,
    UdpDown = 2,
}

impl FrameKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(FrameKind::UdpUp),
            2 => Some(FrameKind::UdpDown),
            _ => None,
        }
    }
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
        if self.buf.len() < HEADER_LEN { return None; }
        let total = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        let kind = FrameKind::from_u8(self.buf[4])?;
        let listener_id = u16::from_be_bytes([self.buf[5], self.buf[6]]);
        if self.buf.len() < total { return None; }
        let (addr, consumed) = read_addr(&self.buf[HEADER_LEN..])?;
        if self.buf.len() < HEADER_LEN + consumed { return None; }
        let datagram = self.buf[HEADER_LEN + consumed..total].to_vec();
        self.buf.drain(..total);
        Some(DatagramFrame { kind, listener_id, addr, datagram })
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
