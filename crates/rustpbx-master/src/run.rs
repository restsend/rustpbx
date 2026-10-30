//! Top-level wiring: bind listeners, start supervisor, handle signals.

use crate::access::Access;
use crate::config::ConfigSubset;
use crate::supervisor::SupCore;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct MasterOpts {
    pub conf: PathBuf,
    pub master_sock: PathBuf,
}

pub async fn run(opts: MasterOpts) -> anyhow::Result<()> {
    let conf = ConfigSubset::load(&opts.conf).map_err(|e| anyhow::anyhow!("{e}"))?;
    let bind_ip = conf.bind_ip().map_err(|e| anyhow::anyhow!("{e}"))?;
    let master_sock = opts.master_sock.clone();

    // Bind public UDP listeners
    let udp_sockets = Access::bind_udp(bind_ip, &conf.all_udp_ports())?;
    let udp_map: HashMap<u16, Arc<tokio::net::UdpSocket>> =
        udp_sockets.iter().map(|(id, s)| (*id, s.clone())).collect();

    let core = SupCore::new(conf.clone(), &opts.conf, &master_sock, udp_map)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let access = Access::new(core.clone());

    // Admin API on a unix socket (status / test / reload); access control
    // = filesystem permissions (0o660). Default: admin.sock next to the
    // worker IPC socket.
    let admin_sock = crate::admin::admin_sock_path(
        &master_sock,
        conf.master.admin_sock.as_deref(),
    );
    crate::admin::spawn_admin(core.clone(), conf.master.clone(), admin_sock)
        .map_err(|e| anyhow::anyhow!("admin api: {e}"))?;

    access.spawn_udp_recvs(udp_sockets);

    // HTTP fd forwarding
    if let Ok(http_addr) = conf.http_addr() {
        access.spawn_tcp_forward(
            http_addr,
            crate::access::ControlStreamKind::Http,
        )?;
    }

    // TCP SIP forwarding
    if let Some(port) = conf.proxy.tcp_port {
        let addr = std::net::SocketAddr::new(bind_ip, port);
        access.spawn_tcp_forward(addr, crate::access::ControlStreamKind::SipTcp)?;
    }

    // HTTPS/WSS: raw TLS forwarding — the fd is passed through untouched and
    // the WORKER terminates TLS (it owns the certificate).
    if let Some(https) = conf.https_addr.clone() {
        if let Ok(addr) = https.parse::<std::net::SocketAddr>() {
            access.spawn_tcp_forward(addr, crate::access::ControlStreamKind::HttpTls)?;
            tracing::info!(%addr, "tls forwarding (https/wss)");
        }
    }

    // IPC server: workers connect here before bootstrap completes.
    rustpbx_master_ipc::set_runtime_handle(tokio::runtime::Handle::current());
    crate::ipc_server::spawn_ipc_server(core.clone(), &master_sock, tokio::runtime::Handle::current())
        .map_err(|e| anyhow::anyhow!("ipc server: {e}"))?;

    // Bootstrap: activate the `current` target or newest release — but
    // first give survivors of a restarted master a window to reconnect and
    // be ADOPTED. Only spawn a fresh generation when nobody showed up.
    const ADOPTION_GRACE: std::time::Duration = std::time::Duration::from_secs(10);
    tracing::info!(grace_secs = 10, "adoption grace: waiting for surviving workers");
    tokio::time::sleep(ADOPTION_GRACE).await;
    if core.status().await["generations"].as_array().map(|g| !g.is_empty()).unwrap_or(false) {
        tracing::info!("surviving workers adopted — skipping fresh bootstrap");
    } else if let Some(release) = crate::supervisor::current_release_or_newest(&core) {
        match core.upgrade_to(&release).await {
            Ok((generation, child)) => {
                crate::supervisor::spawn_reaper(core.clone(), generation, child);
                tracing::info!(release = %release, generation, "bootstrapped");
            }
            Err(e) => tracing::error!(release = %release, "bootstrap failed: {e}"),
        }
    }

    // Signal handling
    tokio::signal::ctrl_c().await.ok();
    core.shutting_down.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}
