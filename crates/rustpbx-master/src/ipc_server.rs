//! IPC server: accepts worker connections on the master unix socket.

use crate::supervisor::{spawn_worker_pump, SupCore};
use rustpbx_master_ipc::{read_magic, AcceptPairer, PairedOutcome};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub fn spawn_ipc_server(
    core: Arc<SupCore>,
    sock_path: &Path,
    rt: tokio::runtime::Handle,
) -> std::io::Result<()> {
    if let Some(parent) = sock_path.parent() { std::fs::create_dir_all(parent)?; }
    // A live master owns this socket — refuse to hijack it. (Stale sockets
    // from crashed runs fail the probe with ECONNREFUSED/ENOENT.)
    if std::os::unix::net::UnixStream::connect(sock_path).is_ok() {
        return Err(std::io::Error::other(format!(
            "master already running ({} is live)",
            sock_path.display()
        )));
    }
    let _ = std::fs::remove_file(sock_path);
    let listener = StdUnixListener::bind(sock_path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o770));
    }
    let accept_fn = move || {
        let mut pairer = AcceptPairer::new();
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Ok((magic, stream)) = read_magic(stream) else { continue };
            let outcome = with_rt_opt(Some(&rt), || pairer.accept(stream, magic));
            if let PairedOutcome::Linked(handle) = outcome {
                let core = core.clone();
                let peer_pid = handle.peer_pid;
                // The accept thread runs outside the runtime: enter it so
                // spawn_worker_pump's tokio::spawn calls succeed.
                with_rt_opt(Some(&rt), move || {
                    spawn_worker_pump(core, None, handle, peer_pid);
                });
            }
        }
    };
    std::thread::Builder::new().name("ipc-accept".into()).spawn(accept_fn)
        .map_err(|e| std::io::Error::other(format!("spawn: {e}")))?;
    Ok(())
}

pub async fn wait_for_socket(path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if path.exists() { return true; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn with_rt_opt<T>(rt: Option<&tokio::runtime::Handle>, f: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current().is_ok() { return f(); }
    let h = rt.expect("no runtime handle");
    let leaked: &'static tokio::runtime::Handle = Box::leak(Box::new(h.clone()));
    let _guard = leaked.enter();
    f()
}
