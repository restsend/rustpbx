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
use crate::releases;
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
    #[serde(default)]
    release: Option<String>,
}

async fn dispatch(state: Arc<(Arc<SupCore>, MasterSubset)>, req: AdminRequest) -> serde_json::Value {
    let (core, master) = (state.0.clone(), state.1.clone());
    match req.cmd.as_str() {
        "status" => {
            let mut status = core.status().await;
            if let Some(obj) = status.as_object_mut() {
                obj.insert("ok".into(), json!(true));
            }
            status
        }
        "test" => match resolve_release(&core, req.release.as_deref()) {
            Ok(release) => match static_validate(&release) {
                Ok(size) => json!({
                    "ok": true,
                    "release": release.name,
                    "size": size,
                    "sha256": release.manifest.sha256,
                }),
                Err(e) => json!({"ok": false, "release": release.name, "error": e}),
            },
            Err(e) => json!({"ok": false, "error": e}),
        },
        "reload" => {
            let release = match resolve_release(&core, req.release.as_deref()) {
                Ok(r) => r,
                Err(e) => return json!({"ok": false, "error": e}),
            };
            if let Err(e) = static_validate(&release) {
                return json!({"ok": false, "release": release.name, "error": e});
            }
            match core.upgrade_to(&release.name).await {
                Ok((generation, child)) => {
                    spawn_reaper(core.clone(), generation, child);
                    // Record the `current` pointer (restart bootstrap) and GC
                    // old releases — best-effort; rotation already succeeded.
                    let current_link = core
                        .releases_dir
                        .parent()
                        .unwrap_or(Path::new("."))
                        .join("current");
                    let _ = releases::activate(&core.releases_dir, &current_link, &release);
                    let status = core.status().await;
                    let live: Vec<u32> = status["generations"]
                        .as_array()
                        .map(|gens| {
                            gens.iter()
                                .filter_map(|g| g["pid"].as_u64().map(|p| p as u32))
                                .collect()
                        })
                        .unwrap_or_default();
                    let _ = releases::gc(
                        &core.releases_dir,
                        &current_link,
                        master.keep_releases(),
                        &live,
                    );
                    json!({"ok": true, "generation": generation, "release": release.name})
                }
                Err(crate::supervisor::SupError::InProgress) => {
                    json!({"ok": false, "error": "upgrade already in progress"})
                }
                Err(e) => json!({"ok": false, "release": release.name, "error": e.to_string()}),
            }
        }
        other => json!({"ok": false, "error": format!("unknown command `{other}`")}),
    }
}

/// Pick the release to operate on: explicit name, else the newest staged.
fn resolve_release(core: &SupCore, name: Option<&str>) -> Result<releases::Release, String> {
    if let Some(n) = name {
        let dir = core.releases_dir.join(n);
        return releases::validate(&core.releases_dir, &dir)
            .map_err(|e| format!("release `{n}`: {e}"));
    }
    releases::discover(&core.releases_dir)
        .into_iter()
        .next()
        .ok_or_else(|| "no staged release found".to_string())
}

/// Static pre-flight (option A): binary present, non-empty, executable,
/// ELF magic, and sha256 matches the staged manifest. Does not boot anything.
fn static_validate(release: &releases::Release) -> Result<u64, String> {
    let bin = &release.binary;
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
    let bytes = std::fs::read(bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    if bytes.len() >= 4 && bytes[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err("not an ELF binary".into());
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(&bytes);
    let actual = format!("{:x}", digest);
    if actual != release.manifest.sha256 {
        return Err(format!(
            "sha256 mismatch: manifest {}, actual {}",
            release.manifest.sha256, actual
        ));
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
