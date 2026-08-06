//! Controlled UDP load generator for the sipflow collector.
//!
//! Mimics the production remote-backend sender profile: MTU-batched UDP
//! datagrams, RTP-dominated traffic (~`--sip-ratio` SIP), a fixed pool of
//! concurrent calls that spreads across shard pipelines via FNV-1a, and a
//! paced target packet rate.
//!
//! ```sh
//! cargo run --release -p rustpbx-sipflow --example sipflow_loadgen -- \
//!   --target 127.0.0.1:9000 --pps 50000 --duration 600
//! ```

use bytes::BufMut;
use rustpbx_sipflow::protocol::{MsgType, Packet, encode_packet_into};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Args {
    target: SocketAddr,
    pps: u32,
    duration_secs: u64,
    calls: usize,
    sip_ratio: f64,
    mtu: usize,
    client_id: u32,
    tick_ms: u64,
    rtp_payload_len: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        target: "127.0.0.1:9000".parse().unwrap(),
        pps: 20000,
        duration_secs: 120,
        calls: 512,
        sip_ratio: 0.02,
        mtu: 1400,
        client_id: 848911921,
        tick_ms: 5,
        rtp_payload_len: 172,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next();
        let mut val = || v.clone().unwrap_or_default();
        match k.as_str() {
            "--target" => a.target = val().parse().expect("--target ADDR:PORT"),
            "--pps" => a.pps = val().parse().expect("--pps N"),
            "--duration" => a.duration_secs = val().parse().expect("--duration SECS"),
            "--calls" => a.calls = val().parse().expect("--calls N"),
            "--sip-ratio" => a.sip_ratio = val().parse().expect("--sip-ratio 0..1"),
            "--mtu" => a.mtu = val().parse().expect("--mtu BYTES"),
            "--client-id" => a.client_id = val().parse().expect("--client-id N"),
            "--tick-ms" => a.tick_ms = val().parse().expect("--tick-ms N"),
            "--rtp-len" => a.rtp_payload_len = val().parse().expect("--rtp-len N"),
            "--help" | "-h" => {
                eprintln!(
                    "usage: sipflow_loadgen [--target ADDR:PORT] [--pps N] [--duration SECS] \
                     [--calls N] [--sip-ratio R] [--mtu B] [--client-id N] [--tick-ms N] [--rtp-len N]"
                );
                std::process::exit(0);
            }
            _ => eprintln!("ignoring unknown arg: {k}"),
        }
    }
    a
}

fn xorshift64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn make_rtp_payload(len: usize) -> Vec<u8> {
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut out = Vec::with_capacity(len);
    out.push(0x80);
    out.push(0x08);
    while out.len() < len {
        out.push((xorshift64(&mut state) & 0xff) as u8);
    }
    out
}

fn make_sip_payload(call_id: &str) -> Vec<u8> {
    format!(
        "INVITE sip:{call_id}@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.172.1.1:5060;branch=z9hG4bK0000000001\r\n\
         From: <sip:alice@example.com>;tag=1928301774\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.172.1.1:5060>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: 142\r\n\r\n\
         v=0\r\no=alice 123456 654321 IN IP4 10.172.1.1\r\n\
         s=-\r\nc=IN IP4 10.172.1.1\r\nt=0 0\r\n\
         m=audio 5004 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n"
    )
    .into_bytes()
}

/// Pre-encoded frame templates. Timestamp lives at a fixed byte offset for
/// the IPv4 packet layout (17..25), patched per send.
struct Frame {
    bytes: Vec<u8>,
    ts_offset: usize,
}

fn build_frame(msg_type: MsgType, src: (IpAddr, u16), dst: (IpAddr, u16), call_id: &str, leg: Option<i32>, payload: Vec<u8>, client_id: u32) -> Frame {
    let packet = Packet {
        msg_type,
        src,
        dst,
        timestamp: 0,
        call_id: Some(call_id.to_string()),
        leg,
        payload: payload.into(),
        client_id,
    };
    let mut bytes = Vec::with_capacity(256);
    encode_packet_into(&mut bytes, &packet);
    // magic2 + ver1 + type1 + family1 + src_ip4 + src_port2 + dst_ip4 + dst_port2
    let ts_offset = 2 + 1 + 1 + 1 + 4 + 2 + 4 + 2;
    Frame { bytes, ts_offset }
}

fn main() {
    let args = parse_args();
    let sock = UdpSocket::bind("0.0.0.0:0").expect("bind sender socket");
    sock.set_nonblocking(false).expect("blocking send");

    let src_a: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::new(10, 172, 1, 1)), 5060);
    let dst: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::new(10, 172, 2, 1)), 5060);
    let media_a: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::new(10, 172, 1, 1)), 5004);
    let media_b: (IpAddr, u16) = (IpAddr::V4(Ipv4Addr::new(10, 172, 1, 1)), 5006);

    // Pre-encode a frame pool. Each call contributes leg0/leg1 RTP frames
    // (round-robin keeps shard spread realistic); a small separate SIP pool
    // is selected with probability `--sip-ratio`.
    let audio = make_rtp_payload(args.rtp_payload_len);
    let mut frames: Vec<Frame> = Vec::with_capacity(args.calls * 2);
    for i in 0..args.calls {
        let call_id = format!("call-{i:06}");
        frames.push(build_frame(MsgType::Rtp, media_a, dst, &call_id, Some(0), audio.clone(), args.client_id));
        frames.push(build_frame(MsgType::Rtp, media_b, dst, &call_id, Some(1), audio.clone(), args.client_id));
    }
    let mut sip_frames: Vec<Frame> = Vec::with_capacity(64);
    for i in 0..64 {
        let call_id = format!("call-{:06}", (i * 37) % args.calls);
        let sip = make_sip_payload(&call_id);
        sip_frames.push(build_frame(MsgType::Sip, src_a, dst, &call_id, None, sip, args.client_id));
    }

    let start_wall = Instant::now();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_micros() as u64;
    let sent = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));

    // Reporter thread
    {
        let sent = sent.clone();
        let running = running.clone();
        std::thread::spawn(move || {
            let mut last = 0u64;
            let mut last_t = Instant::now();
            while running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(5));
                let now = Instant::now();
                let s = sent.load(Ordering::Relaxed);
                let dt = now.duration_since(last_t).as_secs_f64();
                if dt > 0.0 {
                    eprintln!(
                        "loadgen: total_sent={s}  rate={:.0} pps",
                        (s - last) as f64 / dt
                    );
                }
                last = s;
                last_t = now;
            }
        });
    }

    // Pacing loop. `tick_ms` tick; fractional packet budget carries over.
    let tick = Duration::from_millis(args.tick_ms);
    let per_tick = args.pps as f64 * args.tick_ms as f64 / 1000.0;
    let mut budget: f64 = 0.0;
    let mut idx = 0usize;
    let mut tick_ts: u64 = 0;
    let mut dgram: Vec<u8> = Vec::with_capacity(args.mtu + 64);

    let deadline = start_wall + Duration::from_secs(args.duration_secs);
    while Instant::now() < deadline {
        let tick_start = Instant::now();
        tick_ts += 1;
        let now_micros = epoch + tick_ts * args.tick_ms * 1000;

        budget += per_tick;
        let mut to_send = budget.floor() as usize;
        budget -= to_send as f64;

        while to_send > 0 {
            dgram.clear();
            dgram.put_u16(0x5347); // BATCH_MAGIC
            dgram.put_u8(1); // BATCH_VERSION
            let count_pos = dgram.len();
            dgram.put_u16(0);
            let mut n = 0u16;
            while to_send > 0 {
                let is_sip = (idx % 128) < (args.sip_ratio * 128.0).round() as usize;
                let f = if is_sip {
                    &sip_frames[idx % sip_frames.len()]
                } else {
                    &frames[idx % frames.len()]
                };
                idx += 1;
                let frame_len = f.bytes.len();
                if n > 0 && dgram.len() + 4 + frame_len > args.mtu {
                    break;
                }
                dgram.put_u32(frame_len as u32);
                let payload_start = dgram.len();
                dgram.extend_from_slice(&f.bytes);
                // Patch timestamp (IPv4 layout, big-endian).
                dgram[payload_start + f.ts_offset..payload_start + f.ts_offset + 8]
                    .copy_from_slice(&(now_micros + idx as u64).to_be_bytes());
                n += 1;
                to_send -= 1;
            }
            dgram[count_pos..count_pos + 2].copy_from_slice(&n.to_be_bytes());
            let _ = sock.send_to(&dgram, args.target);
            sent.fetch_add(n as u64, Ordering::Relaxed);
        }

        let elapsed = tick_start.elapsed();
        if elapsed < tick {
            std::thread::sleep(tick - elapsed);
        }
    }

    running.store(false, Ordering::Relaxed);
    let total = sent.load(Ordering::Relaxed);
    println!(
        "loadgen done: sent={total} over {:.1}s (avg {:.0} pps)",
        start_wall.elapsed().as_secs_f64(),
        total as f64 / start_wall.elapsed().as_secs_f64()
    );
}
