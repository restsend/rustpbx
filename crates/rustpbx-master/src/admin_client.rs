//! CLI-side client for the admin unix socket (length-prefixed JSON frames,
//! see `admin.rs`). One request per connection.

use serde_json::{json, Value};
use std::path::Path;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

/// Send one command; returns the parsed response object.
pub async fn call(
    sock_path: &Path,
    cmd: &str,
    release: Option<&str>,
) -> Result<Value, String> {
    let mut stream = UnixStream::connect(sock_path)
        .await
        .map_err(|e| {
            format!(
                "cannot reach master admin socket {}: {e} (is the master running?)",
                sock_path.display()
            )
        })?;
    let req = json!({"cmd": cmd, "release": release}).to_string();
    let mut frame = Vec::with_capacity(4 + req.len());
    frame.extend_from_slice(&(req.len() as u32).to_be_bytes());
    frame.extend_from_slice(req.as_bytes());
    stream
        .write_all(&frame)
        .await
        .map_err(|e| format!("write: {e}"))?;

    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| format!("read body: {e}"))?;
    serde_json::from_slice(&buf).map_err(|e| format!("malformed response: {e}"))
}
