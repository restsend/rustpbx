//! Link establishment and fd passing over unix socketpairs.

use crate::control::{decode_control, encode_control, ControlMessage};
use crate::frame::{DataWriter, FrameParser};
use nix::sys::socket::{self, sockopt, ControlMessageOwned, MsgFlags, SockType};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::OnceLock;
use tokio::sync::mpsc;

pub const CONTROL_MAGIC: u8 = b'C';
pub const DATA_MAGIC: u8 = b'D';

static RT_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

pub fn set_runtime_handle(h: tokio::runtime::Handle) {
    let _ = RT_HANDLE.set(h);
}

fn with_rt_ctx<T>(f: impl FnOnce() -> T) -> io::Result<T> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Ok(f());
    }
    let h = RT_HANDLE
        .get()
        .cloned()
        .ok_or_else(|| io::Error::other("no tokio runtime; call set_runtime_handle() first"))?;
    let leaked: &'static tokio::runtime::Handle = Box::leak(Box::new(h));
    let _guard = leaked.enter();
    Ok(f())
}

/// A control message plus any fds that arrived with it.
#[derive(Debug)]
pub struct ControlFrame {
    pub msg: ControlMessage,
    pub fds: Vec<OwnedFd>,
}

#[derive(Debug)]
enum Outgoing {
    Msg(ControlMessage),
    MsgWithFd(ControlMessage, OwnedFd),
}

#[derive(Clone)]
pub struct ControlSender {
    tx: std::sync::mpsc::Sender<Outgoing>,
}

impl ControlSender {
    pub fn send(&self, msg: ControlMessage) -> io::Result<()> {
        self.tx.send(Outgoing::Msg(msg)).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer gone"))
    }

    pub fn send_with_fd(&self, msg: ControlMessage, fd: RawFd) -> io::Result<()> {
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let owned = nix::unistd::dup(borrowed).map_err(|e| io::Error::other(format!("dup: {e}")))?;
        self.tx.send(Outgoing::MsgWithFd(msg, owned)).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "writer gone"))
    }
}

/// One end of an established link.
pub struct LinkHandle {
    pub control_tx: ControlSender,
    pub control_rx: mpsc::Receiver<ControlFrame>,
    pub data: DataWriter,
    pub data_rx: mpsc::Receiver<crate::frame::DatagramFrame>,
    pub peer_pid: Option<u32>,
}

pub fn socketpair_std() -> io::Result<(std::os::unix::net::UnixStream, std::os::unix::net::UnixStream)> {
    let (a, b) = nix::sys::socket::socketpair(
        nix::sys::socket::AddressFamily::Unix,
        SockType::Stream,
        None,
        nix::sys::socket::SockFlag::empty(),
    ).map_err(|e| io::Error::other(format!("socketpair: {e}")))?;
    Ok((
        std::os::unix::net::UnixStream::from(a),
        std::os::unix::net::UnixStream::from(b),
    ))
}

pub fn peer_pid_of(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    socket::getsockopt(&stream, sockopt::PeerCredentials)
        .ok()
        .map(|c: nix::sys::socket::UnixCredentials| c.pid() as u32)
}

pub fn control_send_with_fds(stream: &std::os::unix::net::UnixStream, payload: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    let mut sent = 0usize;
    while sent < payload.len() {
        let iov = [std::io::IoSlice::new(&payload[sent..])];
        let n = if sent == 0 && fd.is_some() {
            let cmsgs = [socket::ControlMessage::ScmRights(&[fd.unwrap()])];
            socket::sendmsg::<nix::sys::socket::SockaddrStorage>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
                .map_err(|e| io::Error::other(format!("sendmsg: {e}")))?
        } else {
            socket::sendmsg::<nix::sys::socket::SockaddrStorage>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)
                .map_err(|e| io::Error::other(format!("sendmsg: {e}")))?
        };
        if n == 0 { return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote 0")); }
        sent += n;
    }
    Ok(())
}

pub fn recvmsg_prefix(stream: &mut std::os::unix::net::UnixStream, buf: &mut [u8], offset: usize, fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    if offset == 0 {
        let mut iov = [std::io::IoSliceMut::new(buf)];
        let mut cmsg = vec![0u8; 128];
        let msg = socket::recvmsg::<nix::sys::socket::SockaddrStorage>(
            stream.as_raw_fd(), &mut iov, Some(&mut cmsg), MsgFlags::empty(),
        ).map_err(|e| io::Error::other(format!("recvmsg: {e}")))?;
        for c in msg.cmsgs()? {
            if let ControlMessageOwned::ScmRights(rights) = c {
                for r in rights {
                    fds.push(unsafe { OwnedFd::from_raw_fd(r) });
                }
            }
        }
        Ok(msg.bytes)
    } else {
        stream.read(buf)
    }
}

pub fn control_recv_with_fds(stream: &mut std::os::unix::net::UnixStream) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut header = [0u8; 4];
    let mut fds = Vec::new();
    let mut got = recvmsg_prefix(stream, &mut header, 0, &mut fds)?;
    // Peer closed the socket: recv/read return 0 forever — that is EOF, not
    // "keep waiting". Without these checks the reader thread spins forever
    // and the worker never learns the master died.
    if got == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
    }
    while got < 4 {
        let n = recvmsg_prefix(stream, &mut header[got..], got, &mut fds)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        got += n;
    }
    let len = u32::from_be_bytes(header) as usize;
    let mut buf = vec![0u8; len];
    let mut have = 0usize;
    while have < len {
        let n = recvmsg_prefix(stream, &mut buf[have..], have + 4, &mut fds)?;
        if n == 0 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof")); }
        have += n;
    }
    Ok((buf, fds))
}

/// Set up one end of a link from pre-connected socketpair halves.
pub fn handle_for_end(
    control: std::os::unix::net::UnixStream,
    data: std::os::unix::net::UnixStream,
    queue_cap: usize,
) -> io::Result<LinkHandle> {
    let _rt = with_rt_ctx(|| ())?;
    let peer_pid = peer_pid_of(&control);
    control.set_nonblocking(false)?;

    // ── control writer thread ──
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Outgoing>();
    let cw = control.try_clone()?;
    std::thread::Builder::new().name("ipc-w".into()).spawn(move || {
        for out in out_rx {
            let r = match out {
                Outgoing::Msg(m) => encode_control(&m).and_then(|b| send_fds(&cw, &b, None)),
                Outgoing::MsgWithFd(m, owned) => {
                    let r = encode_control(&m).and_then(|b| send_fds(&cw, &b, Some(owned.as_raw_fd())));
                    drop(owned);
                    r
                }
            };
            if r.is_err() { break; }
        }
    }).map_err(|e| io::Error::other(format!("spawn: {e}")))?;

    // ── control reader thread ──
    let mut cr = control.try_clone()?;
    let (ctx, crx) = mpsc::channel::<ControlFrame>(256);
    std::thread::Builder::new().name("ipc-r".into()).spawn(move || {
        loop {
            match control_recv_with_fds(&mut cr) {
                Ok((payload, fds)) => match decode_control(&payload) {
                    Ok(msg) => { if ctx.blocking_send(ControlFrame { msg, fds }).is_err() { break; } }
                    Err(e) => { tracing::error!("bad control frame: {e}"); break; }
                },
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break,
            }
        }
    }).map_err(|e| io::Error::other(format!("spawn: {e}")))?;

    // ── data channel (async) ──
    data.set_nonblocking(true)?;
    let tokio_data = tokio::net::UnixStream::from_std(data)?;
    let (read_half, write_half) = tokio_data.into_split();
    let (data_writer, mut frame_rx) = DataWriter::new(queue_cap);
    let (dtx, drx) = mpsc::channel::<crate::frame::DatagramFrame>(1024);

    // flush: queued frames → socket
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut sink = write_half;
        while let Some(frame) = frame_rx.recv().await {
            if let Err(e) = sink.write_all(&frame).await {
                tracing::debug!("data flush error: {e}"); break;
            }
        }
    });

    // pump: socket → parsed datagrams
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut src = read_half;
        let mut parser = FrameParser::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            match src.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    parser.push(&chunk[..n]);
                    while let Some(df) = parser.next_frame() {
                        if dtx.send(df).await.is_err() { return; }
                    }
                }
            }
        }
    });

    Ok(LinkHandle {
        control_tx: ControlSender { tx: out_tx },
        control_rx: crx,
        data: data_writer,
        data_rx: drx,
        peer_pid,
    })
}

fn send_fds(stream: &std::os::unix::net::UnixStream, payload: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    let mut sent = 0usize;
    while sent < payload.len() {
        let iov = [std::io::IoSlice::new(&payload[sent..])];
        let n = if sent == 0 && fd.is_some() {
            let cmsgs = [socket::ControlMessage::ScmRights(&[fd.unwrap()])];
            socket::sendmsg::<nix::sys::socket::SockaddrStorage>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
                .map_err(|e| io::Error::other(format!("sendmsg: {e}")))?
        } else {
            socket::sendmsg::<nix::sys::socket::SockaddrStorage>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)
                .map_err(|e| io::Error::other(format!("sendmsg: {e}")))?
        };
        if n == 0 { return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote 0")); }
        sent += n;
    }
    Ok(())
}

/// Worker side: dial `path` twice and negotiate C/D roles.
pub async fn worker_connect(path: &Path) -> io::Result<LinkHandle> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut control = dial(path).await?;
    let mut data = dial(path).await?;
    control.write_all(&[CONTROL_MAGIC]).await?;
    control.flush().await?;
    data.write_all(&[DATA_MAGIC]).await?;
    data.flush().await?;
    let mut ack = [0u8; 1];
    if control.read(&mut ack).await? != 1 || ack[0] != b'K' {
        return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "no ack"));
    }
    let control_std = control.into_std()?;
    let data_std = data.into_std()?;
    handle_for_end(control_std, data_std, 512)
}

async fn dial(path: &Path) -> io::Result<tokio::net::UnixStream> {
    let mut last_err = None;
    for attempt in 0..50u32 {
        match tokio::net::UnixStream::connect(path).await {
            Ok(s) => return Ok(s),
            Err(e) if attempt < 49 => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("dial failed")))
}

/// Master side: classify accepted connections and pair them into a link.
pub struct AcceptPairer {
    control: Option<(std::os::unix::net::UnixStream, Option<u32>)>,
    data: Option<std::os::unix::net::UnixStream>,
}

pub enum PairedOutcome {
    Waiting,
    Linked(LinkHandle),
}

impl AcceptPairer {
    pub fn new() -> Self { Self { control: None, data: None } }

    pub fn accept(&mut self, stream: std::os::unix::net::UnixStream, magic: u8) -> PairedOutcome {
        match magic {
            CONTROL_MAGIC => {
                let pid = peer_pid_of(&stream);
                self.control = Some((stream, pid));
            }
            DATA_MAGIC => self.data = Some(stream),
            _ => return PairedOutcome::Waiting,
        }
        if self.control.is_none() || self.data.is_none() {
            return PairedOutcome::Waiting;
        }
        let (control, pid) = self.control.take().unwrap();
        let data = self.data.take().unwrap();
        // Acknowledge pairing on the control stream: `worker_connect` blocks
        // on this byte before handing the link to the caller.
        {
            use std::io::Write as _;
            if let Ok(mut ack) = control.try_clone() {
                let _ = ack.write_all(b"K");
            }
        }
        match handle_for_end(control, data, 4096) {
            Ok(mut h) => { h.peer_pid = pid; PairedOutcome::Linked(h) }
            Err(e) => { tracing::error!("handle_for_end: {e}"); PairedOutcome::Waiting }
        }
    }
}

pub fn read_magic(mut stream: std::os::unix::net::UnixStream) -> io::Result<(u8, std::os::unix::net::UnixStream)> {
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte)?;
    Ok((byte[0], stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlMessage;

    #[tokio::test]
    async fn control_roundtrip_between_ends() {
        let (ca, cb) = socketpair_std().unwrap();
        let (da, db) = socketpair_std().unwrap();
        let mut ha = handle_for_end(ca, da, 16).unwrap();
        let hb = handle_for_end(cb, db, 16).unwrap();
        hb.control_tx.send(ControlMessage::Hello { generation: 1, pid: 2, version: "t".into() }).unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ha.control_rx.recv()).await.expect("timeout").expect("closed");
        assert!(matches!(frame.msg, ControlMessage::Hello { generation: 1, .. }));
    }

    #[tokio::test]
    async fn datagrams_flow_between_ends() {
        let (ca, cb) = socketpair_std().unwrap();
        let (da, db) = socketpair_std().unwrap();
        let mut ha = handle_for_end(ca, da, 16).unwrap();
        let mut hb = handle_for_end(cb, db, 16).unwrap();
        let peer: std::net::SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let frame = crate::frame::encode_datagram(crate::frame::FrameKind::UdpUp, 2, &peer, b"REGISTER");
        assert!(ha.data.send(frame));
        let df = tokio::time::timeout(std::time::Duration::from_secs(5), hb.data_rx.recv()).await.expect("timeout").expect("closed");
        assert_eq!(df.listener_id, 2);
        assert_eq!(df.datagram, b"REGISTER");
    }

    #[tokio::test]
    async fn fd_passes_through_control_channel() {
        let (ca, cb) = socketpair_std().unwrap();
        let (da, db) = socketpair_std().unwrap();
        let mut ha = handle_for_end(ca, da, 16).unwrap();
        let hb = handle_for_end(cb, db, 16).unwrap();
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        hb.control_tx.send_with_fd(ControlMessage::Ping { nonce: 9 }, sock.as_raw_fd()).unwrap();

        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ha.control_rx.recv()).await.expect("timeout").expect("closed");
        assert_eq!(frame.msg, ControlMessage::Ping { nonce: 9 });
        assert_eq!(frame.fds.len(), 1);
        let fd: OwnedFd = frame.fds.into_iter().next().unwrap();
        let dup: std::net::UdpSocket = std::net::UdpSocket::from(fd);
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        probe.send_to(b"fd-ok", sock.local_addr().unwrap()).unwrap();
        let mut buf = [0u8; 16];
        let n = dup.recv(&mut buf).expect("recv via dup'd fd");
        assert_eq!(&buf[..n], b"fd-ok");
    }
}

#[cfg(test)]
mod eof_tests {
    use super::*;

    /// Peer close must surface as EOF **immediately** — a missing/incorrect
    /// zero-byte check here spins the reader thread forever and the worker
    /// never learns the master died (found via strace in the restart e2e).
    #[test]
    fn eof_detected_on_peer_close() {
        let (master_end, worker_end) = socketpair_std().unwrap();
        let mut cr = worker_end;
        drop(master_end); // master side closes
        let started = std::time::Instant::now();
        let err = control_recv_with_fds(&mut cr).unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "EOF detection spun instead of returning"
        );
        assert!(
            matches!(err.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset),
            "unexpected kind: {err}"
        );
    }
}
