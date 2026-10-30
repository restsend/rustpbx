//! Admin API on a unix socket — **not HTTP**. Wire protocol (mirrors the
//! worker IPC style): 4-byte big-endian length prefix + JSON payload.
//!
//! ```text
//! request : {"cmd":"status"} | {"cmd":"test","release":"r2"|null}
//!         | {"cmd":"reload","release":"r2"|null}
//! response: {"ok":true, ...} | {"ok":false,"error":"..."}
//! ```
//!
//! Access control = filesystem permissions (0o660). If a master is already
//! running (socket live), a second daemon refuses to start instead of
//! hijacking the socket; CLI commands are pure clients and never bind.

use crate::config::MasterSubset;
use crate::supervisor::{spawn_reaper, SupCore};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const MAX_FRAME: u32 = 1 << 20; // 1 MiB

#[derive(Clone)]
pub struct AdminState {
    pub core: Arc<SupCore>,
    pub master: MasterSubset,
}

/// Admin socket path: explicit config wins, else `admin.sock` next to the
/// worker IPC socket.
pub fn admin_sock_path(master_sock: &Path, configured: Option<&str>) -> PathBuf {
    match configured {
        Some(p) => PathBuf::from(p),
        None => master_sock
            .parent()
            .unwrap_or(Path::new("/tmp"))
            .join("admin.sock"),
    }
}

/// Probe: is a live master listening on this admin socket? A unix connect
/// returns instantly in every case (live → ok, stale file → ECONNREFUSED,
/// missing → ENOENT).
fn admin_is_live(sock_path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(sock_path).is_ok()
}

/// Serve the admin protocol. The socket is bound **synchronously** — the
/// file exists when this returns. Refuses to start when a master is already
/// listening (CLI commands connect as clients; they never bind).
pub fn spawn_admin(
    core: Arc<SupCore>,
    master: MasterSubset,
    sock_path: PathBuf,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if admin_is_live(&sock_path) {
        return Err(std::io::Error::other(format!(
            "master already running ({} is live); refusing to hijack the admin socket",
            sock_path.display()
        )));
    }
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Stale socket from a crashed run: nothing is listening, safe to replace.
    let _ = std::fs::remove_file(&sock_path);
    let std_listener = std::os::unix::net::UnixListener::bind(&sock_path)?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o660))?;
    std_listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(std_listener)?;
    let state = Arc::new((core, master));
    std::thread::Builder::new()
        .name("admin-sock".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("admin runtime");
            runtime.block_on(serve_on(listener, state));
        })
        .map(|_| ())
}

/// Accept loop for an already-bound admin unix listener (shared by the
/// daemon's `spawn_admin` and tests).
pub async fn serve_on(
    listener: tokio::net::UnixListener,
    state: Arc<(Arc<SupCore>, MasterSubset)>,
) {
    tracing::info!(path = %listener.local_addr().map(|a| a.as_pathname().map(|p| p.display().to_string()).unwrap_or_default()).unwrap_or_default(), "admin api listening");
    loop {
        match listener.accept().await {
            Ok((mut stream, _)) => {
                let state = state.clone();
                // Idle/broken clients must not leak the task: bound the
                // request read (response write follows the same deadline).
                tokio::spawn(async move {
                    let read = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        read_frame(&mut stream),
                    )
                    .await;
                    let Ok(Ok(req_bytes)) = read else { return };
                    let resp = match serde_json::from_slice::<AdminRequest>(&req_bytes) {
                        Ok(req) => dispatch(state, req).await,
                        Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
                    };
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        write_frame(&mut stream, resp.to_string().as_bytes()),
                    )
                    .await;
                });
            }
            Err(e) => {
                tracing::warn!("admin accept: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

#[derive(Deserialize)]
struct AdminRequest {
    cmd: String,
}

async fn dispatch(state: Arc<(Arc<SupCore>, MasterSubset)>, req: AdminRequest) -> serde_json::Value {
    let core = state.0.clone();
    match req.cmd.as_str() {
        "status" => {
            let mut status = core.status().await;
            if let Some(obj) = status.as_object_mut() {
                obj.insert("ok".into(), json!(true));
            }
            status
        }
        "test" => match static_validate(&core.worker_bin) {
            Ok(size) => json!({
                "ok": true,
                "bin": core.worker_bin.to_string_lossy(),
                "size": size,
            }),
            Err(e) => json!({"ok": false, "bin": core.worker_bin.to_string_lossy(), "error": e}),
        },
        "reload" => {
            // The NEW worker binary must already be in place at
            // `core.worker_bin` (rename is atomic; see the rotate flow).
            if let Err(e) = static_validate(&core.worker_bin) {
                return json!({"ok": false, "error": e});
            }
            match core.upgrade_to().await {
                Ok((generation, child)) => {
                    spawn_reaper(core.clone(), generation, child);
                    json!({"ok": true, "generation": generation})
                }
                Err(crate::supervisor::SupError::InProgress) => {
                    json!({"ok": false, "error": "upgrade already in progress"})
                }
                Err(e) => json!({"ok": false, "error": e.to_string()}),
            }
        }
        other => json!({"ok": false, "error": format!("unknown command `{other}`")}),
    }
}

/// Static pre-flight (option A): binary present, non-empty, executable,
/// ELF magic. Does not boot anything.
fn static_validate(bin: &Path) -> Result<u64, String> {
    let meta = std::fs::metadata(bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    if meta.len() == 0 {
        return Err("binary is empty".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err("binary is not executable".into());
        }
    }
    // Full read validates the file is readable end-to-end.
    let bytes = std::fs::read(bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    if bytes.len() >= 4 && bytes[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err("not an ELF binary".into());
    }
    Ok(meta.len())
}

async fn read_frame(stream: &mut tokio::net::UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame too large: {len}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame(stream: &mut tokio::net::UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(bytes);
    stream.write_all(&frame).await
}
