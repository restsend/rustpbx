//! Worker lifecycle: spawn → Hello → Ready → Activate → Drain → Drained → exit.
//!
//! The master owns public listeners (see `access.rs`) and routes traffic to the
//! active generation. New generations boot alongside the old one, flip over
//! atomically on `Ready`, and the old one drains its dialogs before exiting.

use crate::config::{ConfigSubset, RtpSlice};
use crate::dialogs::{DialogTable, Metrics};
use rustpbx_master_ipc::frame::{DataWriter, FrameKind};
use rustpbx_master_ipc::link::ControlFrame;
use rustpbx_master_ipc::{ControlMessage, ControlSender, LinkHandle};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

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
    pub udp_sockets: Mutex<HashMap<u16, Arc<tokio::net::UdpSocket>>>,
    pub stats: SupervisorStats,
    pub shutting_down: AtomicBool,
    state: AsyncMutex<SupState>,
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
        Ok(Arc::new(Self {
            dialogs,
            udp_sockets: Mutex::new(udp_sockets),
            stats: SupervisorStats::default(),
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

    // UDP outbound: worker datagrams → master → public socket
    {
        let core = core.clone();
        tokio::spawn(async move {
            while let Some(df) = data_rx.recv().await {
                if df.kind != FrameKind::UdpDown {
                    continue;
                }
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
                        pump.on_dialog_up(g, call_id).await;
                    }
                }
                ControlMessage::DialogDown { call_id } => pump.on_dialog_down(call_id).await,
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
