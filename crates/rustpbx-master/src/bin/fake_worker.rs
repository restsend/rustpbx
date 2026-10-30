//! Minimal fake worker for integration testing: talks the IPC protocol but
//! does no SIP. Sends Hello+Ready, honors Activate/Drain/Stop, and echoes
//! UDP datagrams back as `PONG` so the master relay can be tested end-to-end.

use clap::Parser;
use rustpbx_master_ipc::frame::{encode_datagram, FrameKind};
use rustpbx_master_ipc::{worker_connect, ControlMessage};
use std::time::Duration;

#[derive(Parser, Debug)]
struct Cli {
    #[clap(long, default_value = "config.toml")]
    #[allow(dead_code)]
    conf: std::path::PathBuf,
    /// Master IPC unix socket path (same flag the real rustpbx uses).
    #[clap(long = "master")]
    master_sock: std::path::PathBuf,
    #[clap(long, default_value = "0")]
    worker_gen: u32,
    #[clap(long, default_value = "0")]
    #[allow(dead_code)]
    rtp_start: u16,
    #[clap(long, default_value = "0")]
    #[allow(dead_code)]
    rtp_end: u16,
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let code = rt.block_on(run(cli.master_sock, cli.worker_gen));
    std::process::exit(code);
}

async fn run(master_sock: std::path::PathBuf, worker_gen: u32) -> i32 {
    // Drain delay for testing the drain window, set via the spawner's env
    // (the master passes worker_env entries down to spawned workers).
    let drain_secs: u64 = std::env::var("FAKE_DRAIN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let Ok(link) = worker_connect(&master_sock).await else {
        eprintln!("fake-worker: cannot connect to {}", master_sock.display());
        return 1;
    };
    let control = link.control_tx.clone();
    let mut control_rx = link.control_rx;
    let data_out = link.data.clone();
    let mut data_rx = link.data_rx;
    let pid = std::process::id();

    // UDP echo pump: PING → PONG via the master's public socket.
    tokio::spawn(async move {
        while let Some(df) = data_rx.recv().await {
            if df.kind != FrameKind::UdpUp {
                continue;
            }
            let frame = encode_datagram(FrameKind::UdpDown, df.listener_id, &df.addr, b"PONG");
            let _ = data_out.send(frame);
        }
    });

    if control
        .send(ControlMessage::Hello {
            generation: worker_gen,
            pid,
            version: "fake-1.0".into(),
        })
        .is_err()
    {
        return 1;
    }
    if control.send(ControlMessage::Ready).is_err() {
        return 1;
    }

    while let Some(frame) = control_rx.recv().await {
        match frame.msg {
            ControlMessage::Activate => {
                eprintln!("fake-worker gen{worker_gen}: activated");
            }
            ControlMessage::Drain { timeout_secs } => {
                eprintln!("fake-worker gen{worker_gen}: draining (timeout {timeout_secs})");
                if drain_secs > 0 {
                    tokio::time::sleep(Duration::from_secs(drain_secs)).await;
                }
                let _ = control.send(ControlMessage::Drained);
            }
            ControlMessage::Stop { .. } => {
                eprintln!("fake-worker gen{worker_gen}: stopping");
                return 0;
            }
            ControlMessage::Ping { nonce } => {
                let _ = control.send(ControlMessage::Stats { active_dialogs: nonce });
            }
            ControlMessage::UdpBind { .. } => {}
            _ => {}
        }
    }
    0
}

// keep Duration referenced so the import list stays stable if code moves around
#[allow(dead_code)]
const _: Option<Duration> = None;
