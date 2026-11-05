//! Worker lifecycle: spawn → Hello → Ready → Activate → Drain → Drained → exit.
//!
//! The master owns public listeners (see `access.rs`) and routes traffic to the
//! active generation. New generations boot alongside the old one, flip over
//! atomically on `Ready`, and the old one drains its dialogs before exiting.

use crate::config::{ConfigSubset, RtpSlice};
use crate::conn_table::{ConnTable, Metrics as ConnMetrics};
use crate::registry_table::{RegistryTable, Metrics as RegistryMetrics};
use crate::dialogs::{DialogTable, Metrics};
use rustpbx_master_ipc::frame::{unpack_conn_kind, DataWriter, FrameKind};
use rustpbx_master_ipc::link::ControlFrame;
use rustpbx_master_ipc::{ConnKind, ControlMessage, ControlSender, LinkHandle};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tracing::debug;

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const READY_POLL: Duration = Duration::from_millis(25);
const MAX_RESTARTS_PER_RELEASE: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenState {
    Starting,
    Active,
    Draining,
}

impl GenState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Draining => "draining",
        }
    }
}

/// Per-generation IPC handle handed to the access layer for routing.
pub struct GenLink {
    pub generation: u32,
    pub control: ControlSender,
    pub data: DataWriter,
    pub accepting: AtomicBool,
}

impl GenLink {
    pub fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::SeqCst)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SupError {
    #[error("upgrade already in progress")]
    InProgress,
    #[error("release `{0}`: {1}")]
    Release(String, String),
    #[error("worker not ready: {0}")]
    NotReady(String),
    #[error("no free RTP slice")]
    NoRtpSlice,
    #[error("unknown worker generation {0}")]
    UnknownGeneration(u32),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Default)]
pub struct SupervisorStats {
    pub upgrades_started: AtomicU64,
    pub upgrades_ok: AtomicU64,
    pub upgrades_failed: AtomicU64,
    pub worker_restarts: AtomicU64,
}

struct Generation {
    rtp: RtpSlice,
    state: GenState,
    link: Option<Arc<GenLink>>,
    pid: Option<u32>,
    active_dialogs: u64,
    /// Adopted survivor of a restarted master (not spawned by THIS master).
    /// Adopted generations never auto-drain each other on Ready.
    adopted: bool,
}

#[derive(Default)]
struct SupState {
    gens: HashMap<u32, Generation>,
    current: Option<u32>,
    next_gen: u32,
    /// Crash-restart attempts since the last successful generation.
    restart_attempts: u32,
}

pub struct SupCore {
    pub cfg: ConfigSubset,
    pub conf_path: PathBuf,
    pub master_sock: PathBuf,
    /// Worker binary path. Defaults to `rustpbx` next to this binary; tests
    /// point it at a freshly built worker.
    pub worker_bin: PathBuf,
    /// Extra environment for spawned workers (tests set e.g. FAKE_DRAIN_SECS).
    pub worker_env: Vec<(String, String)>, 
    pub dialogs: Arc<DialogTable>,
    /// Live SIP connections per generation (worker ConnUp/ConnDown reports):
    /// accept-time affinity + relay routing target resolution.
    pub conns: Arc<ConnTable>,
    /// Node-level registration directory (Registry-in-Master): bindings
    /// reported by memory-locator workers so a flip-window dispatch on the
    /// NEW generation can resolve an agent whose connection lives in the
    /// draining one. Unused when the locator is a shared Database.
    pub registry: Arc<RegistryTable>,
    /// `(ConnKind, peer) → generations holding a virtual relay connection`
    /// toward that real connection. Drives the `ConnClosed` fan-out when the
    /// real connection (or its whole generation) dies.
    relay_pairs: Mutex<HashMap<(u8, SocketAddr), HashSet<u32>>>,
    pub udp_sockets: Mutex<HashMap<u16, Arc<tokio::net::UdpSocket>>>,
    pub stats: SupervisorStats,
    pub relay_stats: RelayStats,
    pub shutting_down: AtomicBool,
    state: AsyncMutex<SupState>,
}

/// Cross-gen relay counters (admin status / metrics).
#[derive(Debug, Default)]
pub struct RelayStats {
    pub to_conn_routed: AtomicU64,
    pub to_conn_no_route: AtomicU64,
    pub to_dialog_routed: AtomicU64,
    pub to_dialog_no_route: AtomicU64,
    pub conn_closed_fanout: AtomicU64,
    pub registry_lookup_hits: AtomicU64,
}

impl SupCore {
    pub fn new(
        cfg: ConfigSubset,
        conf_path: &Path,
        master_sock: &Path,
        udp_sockets: HashMap<u16, Arc<tokio::net::UdpSocket>>,
    ) -> Result<Arc<Self>, SupError> {
        let dialogs = Arc::new(DialogTable::new(
            cfg.master.max_dialog_entries(),
            Arc::new(Metrics::default()),
        ));
        let conns = Arc::new(ConnTable::new(
            cfg.master.max_conn_entries(),
            std::time::Duration::from_secs(cfg.master.conn_ttl_secs()),
            Arc::new(ConnMetrics::default()),
        ));
        let registry = Arc::new(RegistryTable::new(
            cfg.master.max_registry_entries(),
            Arc::new(RegistryMetrics::default()),
        ));
        Ok(Arc::new(Self {
            dialogs,
            conns,
            registry,
            relay_pairs: Mutex::new(HashMap::new()),
            udp_sockets: Mutex::new(udp_sockets),
            stats: SupervisorStats::default(),
            relay_stats: RelayStats::default(),
            shutting_down: AtomicBool::new(false),
            state: AsyncMutex::new(SupState {
                gens: HashMap::new(),
                current: None,
                next_gen: 1,
                restart_attempts: 0,
            }),
            cfg,
            conf_path: conf_path.to_path_buf(),
            master_sock: master_sock.to_path_buf(),
            worker_bin: std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("rustpbx")))
                .unwrap_or_else(|| PathBuf::from("rustpbx")),
            worker_env: Vec::new(),
        }))
    }

    /// Point the supervisor at a different worker binary (tests). Must be
    /// called before any generation exists (requires exclusive Arc access).
    pub fn set_worker_bin(self: &mut Arc<Self>, path: PathBuf) {
        match Arc::get_mut(self) {
            Some(this) => this.worker_bin = path,
            None => panic!("set_worker_bin: supervisor already shared"),
        }
    }

    /// Extra env for spawned workers (tests). Same exclusivity contract as
    /// [`Self::set_worker_bin`].
    pub fn set_worker_env(self: &mut Arc<Self>, env: Vec<(String, String)>) {
        match Arc::get_mut(self) {
            Some(this) => this.worker_env = env,
            None => panic!("set_worker_env: supervisor already shared"),
        }
    }

    /// Snapshot for the admin API / tests.
    pub async fn status(&self) -> serde_json::Value {
        let st = self.state.lock().await;
        let mut gens: Vec<serde_json::Value> = st
            .gens
            .iter()
            .map(|(id, g)| {
                serde_json::json!({
                    "generation": id,
                    "state": g.state.as_str(),
                    "pid": g.pid,
                    "rtp_slice": [g.rtp.start, g.rtp.end],
                    "active_dialogs": g.active_dialogs,
                    "accepting": g.link.as_ref().map(|l| l.is_accepting()).unwrap_or(false),
                })
            })
            .collect();
        gens.sort_by_key(|v| v["generation"].as_u64().unwrap_or(0));
        serde_json::json!({
            "current_generation": st.current,
            "generations": gens,
            "dialog_entries": self.dialogs.len(),
            "conn_entries": self.conns.len(),
            "registry_entries": self.registry.len(),
            "relay": {
                "to_conn_routed": self.relay_stats.to_conn_routed.load(Ordering::Relaxed),
                "to_conn_no_route": self.relay_stats.to_conn_no_route.load(Ordering::Relaxed),
                "to_dialog_routed": self.relay_stats.to_dialog_routed.load(Ordering::Relaxed),
                "to_dialog_no_route": self.relay_stats.to_dialog_no_route.load(Ordering::Relaxed),
                "conn_closed_fanout": self.relay_stats.conn_closed_fanout.load(Ordering::Relaxed),
            },
            "counters": {
                "upgrades_started": self.stats.upgrades_started.load(Ordering::Relaxed),
                "upgrades_ok": self.stats.upgrades_ok.load(Ordering::Relaxed),
                "upgrades_failed": self.stats.upgrades_failed.load(Ordering::Relaxed),
                "worker_restarts": self.stats.worker_restarts.load(Ordering::Relaxed),
            },
        })
    }

    /// Route a Call-ID to its owning generation, else the current one.
    pub async fn route_gen(&self, call_id: Option<&str>) -> Option<u32> {
        if let Some(cid) = call_id {
            if let Some(owner) = self.dialogs.owner(cid) {
                return Some(owner);
            }
        }
        self.state.lock().await.current
    }

    pub async fn gen_link(&self, generation: u32) -> Option<Arc<GenLink>> {
        self.state
            .lock()
            .await
            .gens
            .get(&generation)
            .and_then(|g| g.link.clone())
    }

    pub async fn current_gen_link(&self) -> Option<Arc<GenLink>> {
        let st = self.state.lock().await;
        let current = st.current?;
        st.gens.get(&current).and_then(|g| g.link.clone())
    }

    /// Pick a disjoint RTP port slice for a new generation (2-way split).
    fn alloc_rtp_slice(&self, st: &SupState) -> Option<RtpSlice> {
        let start = self.cfg.proxy.rtp_start_port.unwrap_or(12000);
        let end = self.cfg.proxy.rtp_end_port.unwrap_or(42000);
        let total = (end as i64 - start as i64 + 1).max(0);
        let per = total / 2;
        if per <= 0 {
            return None;
        }
        let slices = [
            RtpSlice { start, end: (start as i64 + per - 1) as u16 },
            RtpSlice { start: (start as i64 + per) as u16, end },
        ];
        let used: std::collections::HashSet<(u16, u16)> =
            st.gens.values().map(|g| (g.rtp.start, g.rtp.end)).collect();
        slices.into_iter().find(|s| !used.contains(&(s.start, s.end)))
    }

    /// Boot a staged release as a new generation, wait for Ready, flip over.
    ///
    /// Returns the generation id plus the child process; the caller must
    /// arrange for [`spawn_reaper`] to run (this keeps the future's `Send`
    /// proof acyclic — the reaper path re-enters `on_gen_exited`, which may
    /// restart via this very method).
    pub async fn upgrade_to(self: &Arc<Self>) -> Result<(u32, tokio::process::Child), SupError> {
        self.stats.upgrades_started.fetch_add(1, Ordering::Relaxed);
        let worker_bin = self.worker_bin.clone();
        let meta = std::fs::metadata(&worker_bin).map_err(|e| {
            self.stats.upgrades_failed.fetch_add(1, Ordering::Relaxed);
            SupError::Release(worker_bin.to_string_lossy().into_owned(), e.to_string())
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                self.stats.upgrades_failed.fetch_add(1, Ordering::Relaxed);
                return Err(SupError::Release(
                    worker_bin.to_string_lossy().into_owned(),
                    "not executable".into(),
                ));
            }
        }

        let (generation, rtp) = {
            let mut st = self.state.lock().await;
            if st.gens.values().any(|g| g.state == GenState::Starting) {
                self.stats.upgrades_failed.fetch_add(1, Ordering::Relaxed);
                return Err(SupError::InProgress);
            }
            let generation = st.next_gen;
            st.next_gen += 1;
            let rtp = match self.alloc_rtp_slice(&st) {
                Some(s) => s,
                None => {
                    self.stats.upgrades_failed.fetch_add(1, Ordering::Relaxed);
                    return Err(SupError::NoRtpSlice);
                }
            };
            st.gens.insert(
                generation,
                Generation {
                    rtp,
                    state: GenState::Starting,
                    link: None,
                    pid: None,
                    active_dialogs: 0,
                    adopted: false,
                },
            );
            (generation, rtp)
        };

        let mut cmd = tokio::process::Command::new(&worker_bin);
        cmd.args([
            "--conf",
            &self.conf_path.to_string_lossy(),
            "--master",
            &self.master_sock.to_string_lossy(),
            "--worker-gen",
            &generation.to_string(),
            "--rtp-start",
            &rtp.start.to_string(),
            "--rtp-end",
            &rtp.end.to_string(),
        ])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
        for (k, v) in &self.worker_env {
            cmd.env(k, v);
        }

    // CRITICAL: the worker must NOT inherit the master's file descriptors.
    // Inherited copies of the IPC socketpair keep the control channel "open"
    // after the master dies (the worker never sees EOF → never reconnects),
    // and inherited public-listener fds block the next master's binds.
    #[cfg(unix)]
    {
        // SAFETY: runs in the forked child before exec — only close() is
        // called, which is async-signal-safe. Without this, the worker
        // inherits the master's fds: copies of the IPC socketpair keep the
        // control channel "open" after the master dies (the worker never
        // sees EOF → never reconnects), and inherited listener fds block
        // the next master's binds.
        unsafe {
            cmd.pre_exec(move || {
                // close_range(3, u32::MAX, 0): closes EVERY inherited fd ≥ 3
                // in one syscall (the daemon's accepted-connection fds sit
                // ABOVE 1024 — a 3..1024 loop lets them survive, and the
                // worker then holds the master's IPC ends: no EOF, no
                // reconnect, and the next master can't bind its ports).
                libc::syscall(libc::SYS_close_range, 3u32, libc::c_uint::MAX, 0u32);
                Ok(())
            });
        }
    }

        let mut attempts = 0;
        let spawn_result = loop {
            match cmd.spawn() {
                Ok(c) => break Ok(c),
                // overlayfs/container quirk: execve can race a just-closed
                // write fd and report ETXTBSY; a short retry clears it.
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    attempts += 1;
                    if attempts >= 10 {
                        break Err(e);
                    }
                }
                Err(e) => break Err(e),
            }
        };
        let mut child = match spawn_result {
            Ok(c) => c,
            Err(e) => {
                let mut st = self.state.lock().await;
                st.gens.remove(&generation);
                self.stats.upgrades_failed.fetch_add(1, Ordering::Relaxed);
                return Err(SupError::Release(
                    self.worker_bin.to_string_lossy().into_owned(),
                    format!("spawn: {e}"),
                ));
            }
        };
        let pid = child.id();
        if let Some(g) = self.state.lock().await.gens.get_mut(&generation) {
            g.pid = pid;
        }

        // Wait for Ready, failing fast if the worker dies before reporting
        // (bad args, config error…). Without this the child would linger as
        // an unreaped zombie and the boot would stall until READY_TIMEOUT.
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) {
                let mut st = self.state.lock().await;
                st.gens.remove(&generation);
                self.stats.upgrades_failed.fetch_add(1, Ordering::Relaxed);
                return Err(SupError::Release(
                    self.worker_bin.to_string_lossy().into_owned(),
                    "worker exited before Ready".into(),
                ));
            }
            {
                let st = self.state.lock().await;
                match st.gens.get(&generation) {
                    None => return Err(SupError::NotReady("vanished".into())),
                    Some(g) if g.state != GenState::Starting => break,
                    _ => {}
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SupError::NotReady("timeout".into()));
            }
            tokio::time::sleep(READY_POLL).await;
        }
        Ok((generation, child))
    }

    /// Called by the control pump when a worker presents `Hello`.
    pub async fn attach_worker(
        &self,
        generation: u32,
        peer_pid: u32,
        _version: String,
        link: Arc<GenLink>,
    ) -> Result<(), SupError> {
        let mut st = self.state.lock().await;
        // Unknown generation = a worker that survived a master restart,
        // reconnecting with its original id. ADOPT it (never kill it): its
        // calls are still live and its RTP ports are still its own.
        if !st.gens.contains_key(&generation) {
            tracing::warn!(generation, peer_pid, "adopting surviving worker");
            // Reserve the id range: a later upgrade_to must never collide
            // with an adopted generation id.
            st.next_gen = st.next_gen.max(generation + 1);
            st.gens.insert(
                generation,
                Generation {
                    rtp: RtpSlice { start: 0, end: 0 },
                    state: GenState::Starting,
                    link: None,
                    pid: Some(peer_pid),
                    active_dialogs: 0,
                    adopted: true,
                },
            );
        }
        match st.gens.get_mut(&generation) {
            Some(g) if g.link.is_none() => {
                g.link = Some(link.clone());
                g.pid = Some(peer_pid);
                drop(st);
                let sockets = self
                    .udp_sockets
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                for (id, sock) in sockets.iter() {
                    let local = sock
                        .local_addr()
                        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
                    let _ = link.control.send(ControlMessage::UdpBind {
                        listener_id: *id,
                        local,
                        external: None,
                    });
                }
                // Mirror rebuild: dialogs owned by OTHER generations, so this
                // worker can route messages read from its real connections
                // (mirror hit on a foreign gen → relay; miss → local).
                let snapshot = self.mirror_snapshot_for(generation);
                if !snapshot.is_empty() {
                    let _ = link.control.send(ControlMessage::MirrorSnapshot { entries: snapshot });
                }
                Ok(())
            }
            Some(_) => Err(SupError::UnknownGeneration(generation)),
            None => Err(SupError::UnknownGeneration(generation)),
        }
    }

    /// Worker reports Ready: mark Active, Activate it, drain the previous gen.
    pub async fn on_ready(self: &Arc<Self>, generation: u32) {
        let (old, drain_old, new_adopted) = {
            let mut st = self.state.lock().await;
            let old = st.current;
            let new_adopted = st.gens.get(&generation).map(|g| g.adopted).unwrap_or(false);
            if let Some(g) = st.gens.get_mut(&generation) {
                g.state = GenState::Active;
            }
            if !new_adopted {
                st.restart_attempts = 0;
            }
            if new_adopted {
                // ADOPTION: never displace a live generation and never drain
                // a fellow survivor. Only fill an empty current slot (master
                // just restarted; new calls need a route).
                if old.is_none() {
                    st.current = Some(generation);
                }
                (None, false, true)
            } else if old == Some(generation) {
                (None, false, false)
            } else {
                st.current = Some(generation);
                (old, old.is_some(), false)
            }
        };
        if let Some(link) = self.gen_link(generation).await {
            let _ = link.control.send(ControlMessage::Activate);
            link.accepting.store(true, Ordering::SeqCst);
        }
        if drain_old {
            self.begin_drain(old.expect("drain_old implies old")).await;
        }
        if new_adopted {
            tracing::info!(generation, "adopted generation active");
        } else {
            self.stats.upgrades_ok.fetch_add(1, Ordering::Relaxed);
            tracing::info!(generation, "generation active");
        }
    }

    /// Drain EVERY live generation (docker stop / SIGTERM): reject new
    /// traffic everywhere, then wait until all workers have exited or the
    /// timeout elapses.
    pub async fn drain_all_and_wait(self: &Arc<Self>, timeout: Duration) {
        let this = Arc::clone(self);
        let gens: Vec<u32> = {
            let st = self.state.lock().await;
            st.gens.keys().copied().collect()
        };
        for g in gens {
            this.begin_drain(g).await;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let st = self.state.lock().await;
                if st.gens.is_empty() {
                    tracing::info!("all generations exited — drained");
                    return;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(timeout_secs = timeout.as_secs(), "drain timeout — exiting with live generations");
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Ask a generation to stop accepting new dialogs and finish existing ones.
    pub async fn begin_drain(self: &Arc<Self>, generation: u32) {
        let link = {
            let mut st = self.state.lock().await;
            let link = st.gens.get(&generation).and_then(|g| g.link.clone());
            if let Some(g) = st.gens.get_mut(&generation) {
                if g.state == GenState::Active {
                    g.state = GenState::Draining;
                }
            }
            if st.current == Some(generation) {
                st.current = None;
            }
            link
        };
        if let Some(l) = &link {
            let _ = l.control.send(ControlMessage::Drain { timeout_secs: 300 });
            l.accepting.store(false, Ordering::SeqCst);
        }
        tracing::info!(generation, "drain started");
    }

    /// Worker reports it finished draining: tell it to exit.
    pub async fn on_drained(self: &Arc<Self>, generation: u32) {
        if let Some(link) = self.gen_link(generation).await {
            let _ = link.control.send(ControlMessage::Stop { timeout_secs: 5 });
        }
    }

    pub async fn on_dialog_up(&self, generation: u32, dialog_id: String) {
        let _ = self.dialogs.upsert(&dialog_id, generation);
        // Wire Call-IDs are BARE (`callid@domain`), while the worker reports
        // the full DialogId (`callid-local-remote`). Index both so in-dialog
        // requests (BYE/re-INVITE) resolve by their wire Call-ID.
        if let Some(bare) = wire_call_id(&dialog_id) {
            let _ = self.dialogs.upsert(&bare, generation);
        }
    }

    pub async fn on_dialog_down(&self, dialog_id: String) {
        // Remove ONLY the full DialogId key. The bare wire-Call-ID key must
        // SURVIVE: early→confirmed dialog transitions re-order up/down (the
        // confirmed DialogUp re-adds the bare key, and a blind removal here
        // would strand in-dialog requests like BYE on the wrong generation).
        // Stale bare keys are purged with their generation on exit.
        self.dialogs.remove(&dialog_id);
    }

    /// Full-snapshot reconciliation: prune this generation's stale entries
    /// and re-insert live ones (heals drift, rebuilds after master restart).
    pub async fn on_snapshot(&self, generation: u32, call_ids: Vec<String>) {
        // Protect bare wire-Call-ID keys alongside the full DialogIds —
        // otherwise reconcile prunes them (they are not in the live list).
        let protected = call_ids.iter().flat_map(|id| {
            let mut v = vec![id.clone()];
            if let Some(bare) = wire_call_id(id) {
                v.push(bare);
            }
            v
        });
        let _ = self.dialogs.reconcile_exact(generation, protected);
    }

    pub async fn on_stats(&self, generation: u32, active_dialogs: u64) {
        if let Some(g) = self.state.lock().await.gens.get_mut(&generation) {
            g.active_dialogs = active_dialogs;
        }
    }

    // ── connection ownership + cross-gen relay hub ──

    /// Accept-time fallback target when no routing information applies
    /// (flip window `current = None`): the live generation carrying the most
    /// active dialogs — traffic lands where the work is, instead of being
    /// dropped on the floor.
    pub async fn fallback_gen(&self) -> Option<u32> {
        let st = self.state.lock().await;
        st.gens
            .iter()
            .filter(|(_, g)| g.link.is_some())
            .max_by_key(|(_, g)| g.active_dialogs)
            .map(|(id, _)| *id)
    }

    pub fn on_conn_up(
        &self,
        generation: u32,
        conn_id: u64,
        kind: ConnKind,
        peer: SocketAddr,
        _local: SocketAddr,
    ) {
        self.conns.upsert(kind, peer, conn_id, generation);
    }

    pub async fn on_conn_down(&self, conn_id: u64) {
        if let Some((kind, peer)) = self.conns.remove_by_conn_id(conn_id) {
            // Registrations that rode this connection are unreachable through
            // it — drop their directory entries so lookups stop returning a
            // dead destination.
            self.registry.remove_by_conn(kind, &peer);
            self.fan_out_conn_closed(kind, &peer).await;
        }
    }

    /// RegistryUp from a worker: node-level directory insert/refresh/remove.
    pub fn on_registry_up(
        &self,
        generation: u32,
        aors: &[String],
        kind: ConnKind,
        peer: Option<SocketAddr>,
        expires_secs: u64,
    ) {
        self.registry.upsert(aors, kind, peer, expires_secs, generation);
    }

    /// Dispatch-time directory query from a worker (local locator missed).
    pub fn on_registry_lookup(&self, aor: &str) -> Option<(ConnKind, SocketAddr)> {
        self.registry.lookup(aor).inspect(|_| {
            self.relay_stats.registry_lookup_hits.fetch_add(1, Ordering::Relaxed);
        })
    }

    pub fn on_relay_pair_open(&self, generation: u32, kind: ConnKind, peer: SocketAddr) {
        let mut pairs = self.lock_pairs();
        pairs.entry((kind.as_u8(), peer)).or_default().insert(generation);
    }

    pub fn on_relay_pair_close(&self, generation: u32, kind: ConnKind, peer: SocketAddr) {
        let mut pairs = self.lock_pairs();
        if let Some(holders) = pairs.get_mut(&(kind.as_u8(), peer)) {
            holders.remove(&generation);
            if holders.is_empty() {
                pairs.remove(&(kind.as_u8(), peer));
            }
        }
    }

    /// The real connection died: every generation holding a virtual relay
    /// connection toward it must tear its phantom flow down (its locator
    /// bindings and dialogs hang on it — see `ConnClosed` docs).
    async fn fan_out_conn_closed(&self, kind: ConnKind, peer: &SocketAddr) {
        let holders = self.lock_pairs().remove(&(kind.as_u8(), *peer));
        let Some(holders) = holders else { return };
        for target_gen in holders {
            if let Some(link) = self.gen_link(target_gen).await {
                let _ = link.control.send(ControlMessage::ConnClosed {
                    kind,
                    peer: *peer,
                });
                self.relay_stats
                    .conn_closed_fanout
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// DialogUp broadcast: every OTHER live generation learns who owns
    /// `call_id` (workers keep the mirror to make relay-vs-local decisions).
    pub async fn broadcast_dialog_announce(&self, owner: u32, call_id: &str) {
        let links = self.other_links(owner).await;
        if links.is_empty() {
            debug!(owner, %call_id, "broadcast announce: no other live generations");
            return;
        }
        debug!(owner, %call_id, targets = links.len(), "broadcast announce");
        for (_, control) in links {
            let _ = control.send(ControlMessage::DialogOwnerAnnounce {
                call_id: call_id.to_string(),
                generation: owner,
            });
        }
    }

    /// DialogDown broadcast: drop the mirror entry everywhere.
    pub async fn broadcast_dialog_gone(&self, call_id: &str) {
        let links = self.other_links(u32::MAX).await;
        for (_, control) in links {
            let _ = control.send(ControlMessage::DialogOwnerGone {
                call_id: call_id.to_string(),
            });
        }
    }

    /// Full mirror for a (re)connecting worker: dialogs owned by others.
    pub fn mirror_snapshot_for(&self, generation: u32) -> Vec<(String, u32)> {
        self.dialogs
            .entries_snapshot()
            .into_iter()
            .filter(|(_, g)| *g != generation)
            .collect()
    }

    /// Relay hop 1 (dialog gen → conn gen): route by the ConnTable.
    pub async fn route_relay_to_conn(&self, kind: ConnKind, peer: &SocketAddr, data: &[u8]) {
        let Some(target) = self.conns.owner(kind, peer) else {
            self.relay_stats.to_conn_no_route.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if let Some(link) = self.gen_link(target).await {
            let frame = rustpbx_master_ipc::frame::encode_datagram(
                FrameKind::RelayDeliver,
                kind.as_u8() as u16,
                peer,
                data,
            );
            if link.data.send(frame) {
                self.relay_stats.to_conn_routed.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            self.relay_stats.to_conn_no_route.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Relay hop 2 (conn gen → dialog gen): route by the DialogTable using
    /// the Call-ID scanned out of the message payload.
    pub async fn route_relay_to_dialog(&self, kind: ConnKind, peer: &SocketAddr, data: &[u8]) {
        let Some(call_id) = crate::sip_scan::extract_call_id(data) else {
            self.relay_stats.to_dialog_no_route.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(target) = self.dialogs.owner(&call_id) else {
            self.relay_stats.to_dialog_no_route.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if let Some(link) = self.gen_link(target).await {
            let frame = rustpbx_master_ipc::frame::encode_datagram(
                FrameKind::RelayDeliver,
                kind.as_u8() as u16,
                peer,
                data,
            );
            if link.data.send(frame) {
                self.relay_stats.to_dialog_routed.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            self.relay_stats.to_dialog_no_route.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn other_links(&self, exclude: u32) -> Vec<(u32, ControlSender)> {
        let st = self.state.lock().await;
        st.gens
            .iter()
            .filter(|(id, _)| **id != exclude)
            .filter_map(|(id, g)| g.link.as_ref().map(|l| (*id, l.control.clone())))
            .collect()
    }

    fn lock_pairs(&self) -> std::sync::MutexGuard<'_, HashMap<(u8, SocketAddr), HashSet<u32>>> {
        self.relay_pairs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reaper: clean up after a worker exits; auto-restart on crash.
    pub async fn on_gen_exited(self: &Arc<Self>, generation: u32, code: Option<i32>) {
        let was_draining = {
            let st = self.state.lock().await;
            match st.gens.get(&generation) {
                Some(g) => g.state == GenState::Draining,
                None => return,
            }
        };
        self.dialogs.purge_generation(generation);
        // Its connections are gone with the process: evict from the ConnTable
        // and tell every generation holding a virtual relay connection toward
        // one of them (their dialogs must fail fast, not burn timers).
        for (kind, peer) in self.conns.purge_generation(generation) {
            self.fan_out_conn_closed(kind, &peer).await;
        }
        self.registry.purge_generation(generation);
        {
            let mut pairs = self.lock_pairs();
            for (_, holders) in pairs.iter_mut() {
                holders.remove(&generation);
            }
            pairs.retain(|_, holders| !holders.is_empty());
        }
        {
            let mut st = self.state.lock().await;
            st.gens.remove(&generation);
            if st.current == Some(generation) {
                st.current = None;
            }
        }
        tracing::info!(generation, ?code, "worker exited");
        if self.shutting_down.load(Ordering::SeqCst) || was_draining {
            return;
        }
        // Crash of a non-draining worker → bounded auto-restart from the
        // CURRENT worker binary (a recent rotate intentionally restarts on
        // the new version).
        let attempts = {
            let mut st = self.state.lock().await;
            st.restart_attempts += 1;
            st.restart_attempts
        };
        if attempts > MAX_RESTARTS_PER_RELEASE {
            tracing::error!("restart limit reached");
            return;
        }
        self.stats.worker_restarts.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(attempts, "restarting crashed worker");
        match self.upgrade_to().await {
            // The child owns `kill_on_drop`: dropping it would kill the
            // freshly restarted worker. Always hand it to a reaper.
            Ok((generation, child)) => spawn_reaper(self.clone(), generation, child),
            Err(e) => tracing::error!("restart failed: {e}"),
        }
    }
}


/// The wire Call-ID portion of a rsipstack DialogId string
/// (`{call_id}-{local_tag}-{remote_tag}`): everything before the first `-`.
fn wire_call_id(dialog_id: &str) -> Option<String> {
    dialog_id.split_once('-').map(|(c, _)| c.to_string())
}

/// Reaper: wait for a worker to exit, then run cleanup/restart logic.
/// Callers of [`SupCore::upgrade_to`] must spawn this.
pub fn spawn_reaper(core: Arc<SupCore>, generation: u32, mut child: tokio::process::Child) {
    tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(|s| s.code());
        core.on_gen_exited(generation, code).await;
    });
}

/// Pump one worker link: forward UDP datagrams out, drive the control state machine.
pub fn spawn_worker_pump(
    core: Arc<SupCore>,
    gen_hint: Option<u32>,
    link: LinkHandle,
    peer_pid: Option<u32>,
) {
    let control_tx = link.control_tx.clone();
    let data_writer = link.data.clone();
    let mut data_rx = link.data_rx;
    let mut control_rx = link.control_rx;

    let gl = Arc::new(GenLink {
        generation: gen_hint.unwrap_or(0),
        control: control_tx.clone(),
        data: data_writer,
        accepting: AtomicBool::new(false),
    });

    // UDP outbound: worker datagrams → master → public socket. Relay frames
    // (RelayToConn/RelayToDialog) hop through the hub on the same loop.
    {
        let core = core.clone();
        tokio::spawn(async move {
            while let Some(df) = data_rx.recv().await {
                match df.kind {
                    FrameKind::UdpDown => {
                        let sock = core
                            .udp_sockets
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .get(&df.listener_id)
                            .cloned();
                        if let Some(sock) = sock {
                            let _ = sock.send_to(&df.datagram, df.addr).await;
                        }
                    }
                    FrameKind::RelayToConn => {
                        if let Some(kind) = unpack_conn_kind(df.listener_id) {
                            core.route_relay_to_conn(kind, &df.addr, &df.datagram).await;
                        }
                    }
                    FrameKind::RelayToDialog => {
                        if let Some(kind) = unpack_conn_kind(df.listener_id) {
                            core.route_relay_to_dialog(kind, &df.addr, &df.datagram).await;
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    // Control pump
    let pump = core.clone();
    let gl_ctrl = gl.clone();
    tokio::spawn(async move {
        let mut current: Option<u32> = gen_hint;
        while let Some(ControlFrame { msg, .. }) = control_rx.recv().await {
            match msg {
                ControlMessage::Hello { generation, pid, version } => {
                    if let Some(h) = gen_hint {
                        if h != generation {
                            tracing::warn!(claimed = generation, expected = h, "generation mismatch");
                            break;
                        }
                    }
                    let effective_pid = peer_pid.unwrap_or(pid);
                    if pump
                        .attach_worker(generation, effective_pid, version, gl_ctrl.clone())
                        .await
                        .is_err()
                    {
                        let _ = control_tx.send(ControlMessage::Stop { timeout_secs: 0 });
                        break;
                    }
                    current = Some(generation);
                }
                ControlMessage::Ready => {
                    if let Some(g) = current {
                        pump.on_ready(g).await;
                    }
                }
                ControlMessage::DialogUp { call_id } => {
                    if let Some(g) = current {
                        pump.on_dialog_up(g, call_id.clone()).await;
                        // Tell every other generation who owns this dialog —
                        // their relay decisions depend on the mirror.
                        pump.broadcast_dialog_announce(g, &call_id).await;
                    }
                }
                ControlMessage::DialogDown { call_id } => {
                    pump.on_dialog_down(call_id.clone()).await;
                    pump.broadcast_dialog_gone(&call_id).await;
                }
                ControlMessage::Stats { active_dialogs } => {
                    if let Some(g) = current {
                        pump.on_stats(g, active_dialogs).await;
                    }
                }
                ControlMessage::Snapshot { call_ids } => {
                    if let Some(g) = current {
                        pump.on_snapshot(g, call_ids).await;
                    }
                }
                ControlMessage::ConnUp { conn_id, kind, peer, local } => {
                    if let Some(g) = current {
                        pump.on_conn_up(g, conn_id, kind, peer, local);
                    }
                }
                ControlMessage::ConnDown { conn_id } => pump.on_conn_down(conn_id).await,
                ControlMessage::RegistryUp { aors, kind, peer, expires_secs } => {
                    if let Some(g) = current {
                        pump.on_registry_up(g, &aors, kind, peer, expires_secs);
                    }
                }
                ControlMessage::RegistryLookup { nonce, aor } => {
                    let dest = pump.on_registry_lookup(&aor);
                    // Reply on the REQUESTING generation's own link.
                    if let Some(g) = current
                        && let Some(link) = core.gen_link(g).await {
                        let _ = link.control.send(ControlMessage::RegistryLookupReply { nonce, dest });
                    }
                }
                ControlMessage::RelayPairOpen { kind, peer } => {
                    if let Some(g) = current {
                        pump.on_relay_pair_open(g, kind, peer);
                    }
                }
                ControlMessage::RelayPairClose { kind, peer } => {
                    if let Some(g) = current {
                        pump.on_relay_pair_close(g, kind, peer);
                    }
                }
                ControlMessage::Drained => {
                    if let Some(g) = current {
                        pump.on_drained(g).await;
                    }
                }
                ControlMessage::Bye { reason } => {
                    tracing::info!(%reason, "worker said bye");
                    break;
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod hub_tests {
    use super::*;
    use crate::config::ConfigSubset;
    use rustpbx_master_ipc::{socketpair_std, handle_for_end};
    use std::net::IpAddr;

    fn peer(p: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::from([10, 0, 0, 1]), p)
    }

    fn core() -> Arc<SupCore> {
        SupCore::new(ConfigSubset::default(), Path::new("/tmp/x.toml"), Path::new("/tmp/x.sock"), HashMap::new())
            .expect("sup core")
    }

    /// Register a fake generation with a real socketpair link; returns the
    /// worker-side handle whose control_rx/data_rx receive what the hub sends.
    async fn add_gen(core: &Arc<SupCore>, id: u32, active_dialogs: u64) -> LinkHandle {
        let (ca, cb) = socketpair_std().unwrap();
        let (da, db) = socketpair_std().unwrap();
        let rx = handle_for_end(ca, da, 64).unwrap();
        let tx = handle_for_end(cb, db, 64).unwrap();
        let gl = Arc::new(GenLink {
            generation: id,
            control: tx.control_tx.clone(),
            data: tx.data.clone(),
            accepting: AtomicBool::new(true),
        });
        let mut st = core.state.lock().await;
        st.gens.insert(id, Generation {
            rtp: RtpSlice { start: 0, end: 0 },
            state: GenState::Active,
            link: Some(gl),
            pid: None,
            active_dialogs,
            adopted: false,
        });
        rx
    }

    #[tokio::test]
    async fn relay_to_conn_routes_by_conn_table() {
        let c = core();
        let mut rx = add_gen(&c, 2, 0).await;
        c.conns.upsert(ConnKind::Wss, peer(7000), 11, 2);
        c.route_relay_to_conn(ConnKind::Wss, &peer(7000), b"BYE sip:x SIP/2.0\r\n\r\n").await;
        let df = tokio::time::timeout(std::time::Duration::from_secs(5), rx.data_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(df.kind, FrameKind::RelayDeliver);
        assert_eq!(unpack_conn_kind(df.listener_id), Some(ConnKind::Wss));
        assert_eq!(df.datagram, b"BYE sip:x SIP/2.0\r\n\r\n");
        assert_eq!(c.relay_stats.to_conn_routed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn relay_to_conn_without_route_counts_no_route() {
        let c = core();
        add_gen(&c, 2, 0).await;
        c.route_relay_to_conn(ConnKind::Ws, &peer(9999), b"BYE").await;
        assert_eq!(c.relay_stats.to_conn_no_route.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn relay_to_dialog_routes_by_call_id_scan() {
        let c = core();
        let mut rx = add_gen(&c, 5, 0).await;
        c.dialogs.upsert("cid-9", 5).unwrap();
        let sip = b"BYE sip:x SIP/2.0\r\nCall-ID: cid-9\r\n\r\n";
        c.route_relay_to_dialog(ConnKind::Ws, &peer(5000), sip).await;
        let df = tokio::time::timeout(std::time::Duration::from_secs(5), rx.data_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(df.kind, FrameKind::RelayDeliver);
        assert_eq!(df.datagram, sip);
        assert_eq!(c.relay_stats.to_dialog_routed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn dialog_announce_and_mirror_snapshot() {
        let c = core();
        let mut rx1 = add_gen(&c, 1, 0).await;
        let mut rx2 = add_gen(&c, 2, 0).await;
        // gen 1 owns the dialog → only gen 2 (the other gen) gets the announce.
        c.broadcast_dialog_announce(1, "abc").await;
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), rx2.control_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(frame.msg, ControlMessage::DialogOwnerAnnounce { call_id: "abc".into(), generation: 1 });
        c.dialogs.upsert("abc", 1).unwrap();
        c.dialogs.upsert("other-gen-dialog", 2).unwrap();
        let mirror = c.mirror_snapshot_for(2);
        assert_eq!(mirror, vec![("abc".to_string(), 1)]);
        // GONE is broadcast to every live generation, including the owner.
        c.broadcast_dialog_gone("abc").await;
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), rx1.control_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(frame.msg, ControlMessage::DialogOwnerGone { call_id: "abc".into() });
    }

    #[tokio::test]
    async fn conn_down_fans_closed_to_virtual_holders() {
        let c = core();
        let mut holder_rx = add_gen(&c, 3, 0).await; // holds a virtual conn
        let _owner_rx = add_gen(&c, 4, 0).await; // owns the real conn
        c.on_conn_up(4, 77, ConnKind::Wss, peer(8000), peer(0));
        c.on_relay_pair_open(3, ConnKind::Wss, peer(8000));
        c.on_conn_down(77).await;
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), holder_rx.control_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(frame.msg, ControlMessage::ConnClosed { kind: ConnKind::Wss, peer: peer(8000) });
        assert_eq!(c.relay_stats.conn_closed_fanout.load(Ordering::Relaxed), 1);
        // The pair registry was consumed — a second down fans nothing.
        c.on_conn_up(4, 78, ConnKind::Wss, peer(8000), peer(0));
        c.on_conn_down(78).await;
        assert_eq!(c.relay_stats.conn_closed_fanout.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn gen_exit_purges_conns_and_fans_out() {
        let c = core();
        let mut holder_rx = add_gen(&c, 3, 0).await;
        let _owner_rx = add_gen(&c, 4, 0).await;
        c.on_conn_up(4, 90, ConnKind::SipTcp, peer(9000), peer(0));
        c.on_relay_pair_open(3, ConnKind::SipTcp, peer(9000));
        c.on_gen_exited(4, Some(0)).await;
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), holder_rx.control_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(frame.msg, ControlMessage::ConnClosed { kind: ConnKind::SipTcp, peer: peer(9000) });
        assert_eq!(c.conns.owner(ConnKind::SipTcp, &peer(9000)), None);
    }

    #[tokio::test]
    async fn fallback_gen_picks_most_active_with_link() {
        let c = core();
        add_gen(&c, 1, 4).await;
        add_gen(&c, 2, 9).await;
        assert_eq!(c.fallback_gen().await, Some(2));
        // A generation without a link never wins the fallback.
        {
            let mut st = c.state.lock().await;
            if let Some(g) = st.gens.get_mut(&2) { g.link = None; }
        }
        assert_eq!(c.fallback_gen().await, Some(1));
    }
}

#[cfg(test)]
mod pump_wiring_tests {
    use super::*;
    use crate::config::ConfigSubset;
    use rustpbx_master_ipc::{handle_for_end, socketpair_std};

    /// spawn_worker_pump wiring: a worker's DialogUp must (a) update the
    /// DialogTable and (b) broadcast the announce to every OTHER live
    /// generation's control channel; DialogDown must broadcast GONE.
    #[tokio::test]
    async fn dialog_up_broadcasts_announce_to_other_gens() {
        let core = SupCore::new(
            ConfigSubset::default(),
            Path::new("/tmp/x.toml"),
            Path::new("/tmp/x.sock"),
            HashMap::new(),
        )
        .expect("sup core");
        // The "other" generation that must receive the announce.
        let (ca, cb) = socketpair_std().unwrap();
        let (da, db) = socketpair_std().unwrap();
        let mut other_rx = handle_for_end(ca, da, 64).unwrap();
        let other_tx = handle_for_end(cb, db, 64).unwrap();
        {
            let mut st = core.state.lock().await;
            st.gens.insert(2, Generation {
                rtp: RtpSlice { start: 0, end: 0 },
                state: GenState::Active,
                link: Some(Arc::new(GenLink {
                    generation: 2,
                    control: other_tx.control_tx.clone(),
                    data: other_tx.data.clone(),
                    accepting: AtomicBool::new(true),
                })),
                pid: None,
                active_dialogs: 0,
                adopted: false,
            });
        }
        // The reporting worker (gen 1). `worker_tx` is the MASTER-side end the
        // pump reads; frames must be sent from the OPPOSITE end (`worker_rx`)
        // to reach the pump's control_rx.
        let (ca2, cb2) = socketpair_std().unwrap();
        let (da2, db2) = socketpair_std().unwrap();
        let mut worker_rx = handle_for_end(ca2, da2, 64).unwrap();
        let worker_tx = handle_for_end(cb2, db2, 64).unwrap();
        spawn_worker_pump(core.clone(), Some(1), worker_tx, None);

        let control = worker_rx.control_tx.clone();
        control
            .send(ControlMessage::Hello { generation: 1, pid: 42, version: "t".into() })
            .unwrap();
        control.send(ControlMessage::DialogUp { call_id: "c-1".into() }).unwrap();

        // The announce lands on gen 2's control channel…
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), other_rx.control_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(frame.msg, ControlMessage::DialogOwnerAnnounce { call_id: "c-1".into(), generation: 1 });
        // …and the DialogTable learned the owner.
        assert_eq!(core.dialogs.owner("c-1"), Some(1));

        control.send(ControlMessage::DialogDown { call_id: "c-1".into() }).unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), other_rx.control_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(frame.msg, ControlMessage::DialogOwnerGone { call_id: "c-1".into() });
        assert_eq!(core.dialogs.owner("c-1"), None);

        // MirrorSnapshot arrives on Hello (after UdpBind messages — there
        // are none configured, so it is the first frame the worker sees).
        let _ = worker_rx.control_rx.recv(); // drain whatever ordering yields
        // (Hello-time snapshot was empty — dialogs owned by gen 1 are not
        // mirrored to gen 1; nothing more to assert here.)
    }

    /// Relay frames from the worker's data channel are routed by the hub:
    /// RelayToDialog → DialogTable → other gen's data channel.
    #[tokio::test]
    async fn relay_to_dialog_frame_flows_through_pump() {
        let core = SupCore::new(
            ConfigSubset::default(),
            Path::new("/tmp/x.toml"),
            Path::new("/tmp/x.sock"),
            HashMap::new(),
        )
        .expect("sup core");
        // Dialog owner gen 3.
        let (ca, cb) = socketpair_std().unwrap();
        let (da, db) = socketpair_std().unwrap();
        let mut owner_rx = handle_for_end(ca, da, 64).unwrap();
        let owner_tx = handle_for_end(cb, db, 64).unwrap();
        core.dialogs.upsert("cid-relay", 3).unwrap();
        {
            let mut st = core.state.lock().await;
            st.gens.insert(3, Generation {
                rtp: RtpSlice { start: 0, end: 0 },
                state: GenState::Active,
                link: Some(Arc::new(GenLink {
                    generation: 3,
                    control: owner_tx.control_tx.clone(),
                    data: owner_tx.data.clone(),
                    accepting: AtomicBool::new(true),
                })),
                pid: None,
                active_dialogs: 0,
                adopted: false,
            });
        }
        // The conn-owner gen 1 relays an in-dialog message. Same end
        // discipline: drive the pump from the opposite handle's senders.
        let (ca2, cb2) = socketpair_std().unwrap();
        let (da2, db2) = socketpair_std().unwrap();
        let worker_rx = handle_for_end(ca2, da2, 64).unwrap();
        let worker_tx = handle_for_end(cb2, db2, 64).unwrap();
        spawn_worker_pump(core.clone(), Some(1), worker_tx, None);
        let data = worker_rx.data.clone();
        let sip = b"BYE sip:u@d SIP/2.0\r\nCall-ID: cid-relay\r\n\r\n";
        let frame = rustpbx_master_ipc::frame::encode_datagram(
            rustpbx_master_ipc::frame::FrameKind::RelayToDialog,
            rustpbx_master_ipc::ConnKind::Ws.as_u8() as u16,
            &"198.51.100.5:40000".parse().unwrap(),
            sip,
        );
        assert!(data.send(frame));

        let df = tokio::time::timeout(std::time::Duration::from_secs(5), owner_rx.data_rx.recv())
            .await.expect("timeout").expect("closed");
        assert_eq!(df.kind, rustpbx_master_ipc::frame::FrameKind::RelayDeliver);
        assert_eq!(
            rustpbx_master_ipc::frame::unpack_conn_kind(df.listener_id),
            Some(rustpbx_master_ipc::ConnKind::Ws)
        );
        assert_eq!(df.datagram, sip);
        assert_eq!(core.relay_stats.to_dialog_routed.load(Ordering::Relaxed), 1);
    }
}
