"""RustPBX process lifecycle management for E2E tests.

Builds (if needed), starts, health-checks, and tears down the `rustpbx`
binary with a generated config. Also provides an async REST client for
the console / CC phone APIs.
"""

from __future__ import annotations

import asyncio
import logging
import os
import shutil
import signal
import socket
import subprocess
import threading
import time
import urllib.request
import urllib.error
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

import aiohttp

from .config_builder import ConfigBuilder

logger = logging.getLogger(__name__)


# One full-featured binary shared by every E2E worker/session. Building this
# once (console + all addons used by the suite) means no session ever needs to
# recompile: `cargo build` only runs when the stable copy is missing.
#
# NOTE: `addon-sbc` is not a Cargo feature in this workspace (the sbc addon is
# not gated behind a feature), so it must not be passed to `--features`.
# commerce is required for the AMI /cluster/* control plane (dual-node suites)
FULL_E2E_FEATURES = ["default", "contact-center", "addon-wholesale", "commerce"]

# Acceptance 3.2.1.2: replace-mode overflow pair (ovf-prime → ovf-backup at
# max_wait_secs=5). `overflow_mode = "replace"` narrows each escalation step
# to the overflow group only, so a newly-idle ovf-prime agent must NOT pick
# up a call that already overflowed.
_CHECKLIST_SKILL_GROUPS_TOML = """\
[[skill_groups]]
skill_group_id = "ovf-prime"
display_name = "Checklist overflow primary"
skills_required = ["ovf-prime"]
overflow_groups = ["ovf-backup"]
max_wait_secs = 5
overflow_mode = "replace"
sla_target_secs = 30

[[skill_groups]]
skill_group_id = "ovf-backup"
display_name = "Checklist overflow backup"
skills_required = ["ovf-backup"]
overflow_groups = []
max_wait_secs = 90
sla_target_secs = 30
"""

# ACD engine policies for the checklist suites:
#  - "default": longest-idle strategy; binds groups to the inline engine queue
#    so multiple queued calls are dispatched strictly by waiting time ( FIFO
#    tie-break) — acceptance 3.2.1.2 user-queue-duration / cross-day rows.
#  - "ext_fail": external routing pointing at a dead endpoint with local
#    longest-idle fallback — acceptance 3.2.1.2 Vision-failure fallback row.
_CHECKLIST_ACD_TOML_TEMPLATE = """\
enabled = true
default_policy = "default"

[policies.default]
name = "default"

# 00:00-23:59 window — without an explicit schedule the engine default
# (09:00-18:00 Asia/Shanghai) blocks dispatch outside office hours
# ("ACD blocked: off hours") and nightly regression runs fail.
[policies.default.schedule.business_hours]
start = "00:00"
end = "23:59"

[policies.ext_fail]
name = "ext_fail"

[policies.ext_fail.schedule.business_hours]
start = "00:00"
end = "23:59"

[policies.ext_fail.strategy]
strategy_type = "external"

[policies.ext_fail.external]
url = "http://127.0.0.1:{http_port}/nonexistent-acd-callback"
timeout_secs = 2
"""


def find_project_root(start: Optional[Path] = None) -> Path:
    p = (start or Path(__file__)).resolve()
    while p != p.parent:
        if (p / "Cargo.toml").exists() and (p / "target").exists():
            return p
        p = p.parent
    # Fallback: just find Cargo.toml
    p = (start or Path(__file__)).resolve()
    while p != p.parent:
        if (p / "Cargo.toml").exists():
            return p
        p = p.parent
    return Path.cwd()


def find_or_build_binary(project_root: Path, features: Optional[list[str]] = None) -> Path:
    # Explicit override (e.g. CI with a prebuilt, feature-complete binary):
    # trusted as-is, never built over.
    override = os.environ.get("RUSTPBX_E2E_BIN")
    if override:
        p = Path(override)
        if p.is_file():
            logger.info("Using RUSTPBX_E2E_BIN binary at %s", p)
            return p
        logger.warning("RUSTPBX_E2E_BIN=%s does not exist; ignoring", override)

    # Prefer a stable, feature-complete binary copy that survives concurrent
    # `cargo build` invocations overwriting target/debug/rustpbx. Reusing it
    # avoids a multi-minute full rebuild of the ~1GB binary on every session.
    stable = project_root / "target" / "debug" / "rustpbx-cc-e2e"
    if stable.exists() and stable.is_file():
        logger.info("Using stable cc binary at %s", stable)
        return stable

    # NOTE: do NOT silently fall back to target/debug/rustpbx — a plain
    # `cargo build` binary lacks the addon features, boots without the
    # /console/cc/dev or /healthz routes, and makes every readiness poll
    # time out (90s per test). Build with the full feature set instead.
    target = project_root / "target" / "debug" / "rustpbx"
    logger.info("Stable binary missing, building rustpbx with full features (may take a while)...")
    feat = ",".join(features or FULL_E2E_FEATURES)
    result = subprocess.run(
        ["cargo", "build", "--features", feat],
        cwd=str(project_root),
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        logger.error("cargo build failed:\n%s", result.stderr[-2000:])
        raise RuntimeError("Failed to build rustpbx")
    if not target.exists():
        raise RuntimeError(f"Binary not found at {target} after build")
    # Promote to the stable copy so future sessions skip the build entirely.
    try:
        shutil.copy2(target, stable)
        logger.info("Promoted build to stable copy at %s", stable)
    except OSError as exc:
        logger.warning("Could not copy stable binary: %s", exc)
        return target
    return stable


def pick_free_port(host: str = "127.0.0.1", start: int = 0) -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind((host, start))
        return s.getsockname()[1]


@dataclass
class PbxServer:
    """Manages the rustpbx process for a test session."""

    host: str = "127.0.0.1"
    sip_port: int = 5060
    http_port: int = 8080
    rwi_token: str = "test-api-key-e2e"
    project_root: Path = field(default_factory=find_project_root)
    work_dir: Path = field(default_factory=lambda: Path.cwd())
    binary: Optional[Path] = None
    process: Optional[subprocess.Popen] = None
    config_path: Optional[Path] = None
    log_file_path: Optional[Path] = None
    _config_builder: Optional[ConfigBuilder] = None
    _http_session: Optional[aiohttp.ClientSession] = None
    # supervised-restart state (see PbxServer.start)
    _stopping: bool = False
    _respawns: int = 0
    _generation: int = 0
    _swap_lock: threading.Lock = threading.Lock()
    _spawn_cmd: Optional[list] = None
    _spawn_log_f: Optional[object] = None

    @property
    def sip_addr(self) -> str:
        return f"{self.host}:{self.sip_port}"

    @staticmethod
    def _port_holder(port: int) -> Optional[dict]:
        """Return {pid, cmdline} when `port` is bound (LISTEN/UDP), else None.
        Client-side connections TO the port don't count."""
        import subprocess as _sp

        try:
            out = _sp.run(
                ["lsof", "-nP", f"-iTCP:{port}", f"-iUDP:{port}"],
                capture_output=True, text=True, timeout=5,
            ).stdout
        except Exception:  # noqa: BLE001 — lsof missing/timeout: skip the guard
            return None
        for line in out.splitlines()[1:]:
            parts = line.split()
            if len(parts) < 9 or not parts[1].isdigit():
                continue
            name_and_state = " ".join(parts[8:])
            if "->" in name_and_state:  # client side of a connection
                continue
            local = name_and_state.split(" (")[0]
            if not local.endswith(f":{port}"):
                continue
            pid = int(parts[1])
            cmdline = _sp.run(
                ["ps", "-p", str(pid), "-o", "command="],
                capture_output=True, text=True,
            ).stdout.strip()
            return {"pid": pid, "cmdline": cmdline}
        return None

    @property
    def http_url(self) -> str:
        return f"http://{self.host}:{self.http_port}"

    @property
    def ws_url(self) -> str:
        return f"ws://{self.host}:{self.http_port}/ws"

    @property
    def rwi_ws_url(self) -> str:
        return f"ws://{self.host}:{self.http_port}/rwi/v1"

    @property
    def console_url(self) -> str:
        return f"{self.http_url}/console"

    @property
    def config_builder(self) -> ConfigBuilder:
        if self._config_builder is None:
            self._config_builder = ConfigBuilder(
                project_root=self.project_root,
                work_dir=self.work_dir,
                sip_port=self.sip_port,
                http_port=self.http_port,
                rwi_token=self.rwi_token,
            )
        return self._config_builder

    # ---- lifecycle ----

    def prepare(
        self,
        *,
        webhook_url: str = "",
        extra_features: Optional[list[str]] = None,
        build: bool = True,
    ) -> Path:
        """Build binary, generate config, return config path.

        Uses the full E2E feature set (default + contact-center + addon-sbc +
        addon-wholesale) so every addon used by the suite is compiled in. The
        stable `target/debug/rustpbx-cc-e2e` copy is reused when present —
        building only happens once per machine.
        """
        features = list(extra_features) if extra_features else []
        if build:
            self.binary = find_or_build_binary(self.project_root, features)
        elif self.binary is None:
            override = os.environ.get("RUSTPBX_E2E_BIN")
            stable = self.project_root / "target" / "debug" / "rustpbx-cc-e2e"
            if override and Path(override).is_file():
                self.binary = Path(override)
            elif stable.exists():
                self.binary = stable
            else:
                self.binary = find_or_build_binary(self.project_root, features)

        self.config_builder.webhook_url = webhook_url

        # write agents/skill groups/acd files
        cc_dir = self.work_dir / "config" / "cc"
        cc_dir.mkdir(parents=True, exist_ok=True)
        agents_file = cc_dir / "e2e_agents.toml"
        skill_groups_file = cc_dir / "e2e_skill_groups.toml"
        acd_file = cc_dir / "e2e_acd_policies.toml"

        from .config_builder import (
            default_agents_toml,
            default_skill_groups_toml,
            default_acd_policies_toml,
        )

        agents_file.write_text(default_agents_toml(), encoding="utf-8")
        skill_groups_file.write_text(default_skill_groups_toml(), encoding="utf-8")
        acd_file.write_text(default_acd_policies_toml(), encoding="utf-8")

        self.config_builder.set_agents_file("config/cc/e2e_agents.toml")
        self.config_builder.set_skill_groups_file("config/cc/e2e_skill_groups.toml")
        self.config_builder.set_acd_file("config/cc/e2e_acd_policies.toml")

        # ── acceptance checklist (cc/checklist.md 3.2.1.2/3.2.3) extras ──
        # Replace-mode overflow group pair: ovf-prime overflows to ovf-backup
        # after max_wait_secs and (mode=replace) the primary group must NOT
        # keep scheduling the call. Loaded via the CC skill-groups TOML dir.
        skill_groups_dir = cc_dir / "skill_groups"
        skill_groups_dir.mkdir(parents=True, exist_ok=True)
        (skill_groups_dir / "checklist_ovf.toml").write_text(
            _CHECKLIST_SKILL_GROUPS_TOML, encoding="utf-8"
        )
        # ACD engine config: enables the inline engine queue so caller-ordering
        # (longest waiting first) is policy-driven for groups bound to a policy.
        # The ext_fail callback targets this pbx's own HTTP port (unknown path
        # ⇒ 404 ⇒ non-2xx ⇒ local fallback strategy), mirroring the proven
        # "HTTP returned 5xx" degrade pattern.
        (cc_dir / "acd.toml").write_text(
            _CHECKLIST_ACD_TOML_TEMPLATE.format(http_port=self.http_port),
            encoding="utf-8",
        )

        self.config_path = self.config_builder.build()
        logger.info("Config written to %s", self.config_path)
        return self.config_path

    def start(self, timeout: float = 30) -> None:
        """Start rustpbx process and wait until healthy.

        Idempotent (re)start: function-scoped fixtures (cc_api etc.) call
        boot_pbx() per test after mutating the config on the SAME PbxServer
        the session fixture already started. The live instance must be torn
        down first — a duplicate loses the port race, and the port guard
        below would then raise (or, pre-guard, the stale instance would
        poison the rest of the session).
        """
        if self.process is not None and self.process.poll() is None:
            self.stop()
        assert self.binary is not None, "Call prepare() first"
        assert self.config_path is not None, "Call prepare() first"

        # ensure work dirs exist
        for d in ["config/cdr", "config/sipflow", "tests/logs"]:
            (self.work_dir / d).mkdir(parents=True, exist_ok=True)

        # When running from an isolated work dir (not the project root),
        # symlink the repo asset directories so relative paths in IVR/console
        # configs (sounds/..., locales, templates) still resolve. Generated
        # config/* files stay inside the work dir — the developer's own
        # config/ directory is never touched.
        work_dir = self.work_dir.resolve()
        project_root = self.project_root.resolve()
        if work_dir != project_root:
            for asset in ("sounds", "locales", "templates"):
                target = project_root / asset
                link = work_dir / asset
                if target.is_dir() and not link.exists():
                    try:
                        link.symlink_to(target, target_is_directory=True)
                    except (OSError, NotImplementedError):
                        # Best effort only — tests that need the asset dir
                        # will fail with a clear file-not-found otherwise.
                        logger.warning("could not symlink %s into %s", asset, work_dir)
            # /console/cc/desk|dev serve src/addons/cc/static/*.html relative
            # to the rustpbx CWD — mirror the nested path too.
            cc_static = project_root / "src" / "addons" / "cc" / "static"
            nested_parent = work_dir / "src" / "addons" / "cc"
            nested = nested_parent / "static"
            if cc_static.is_dir() and not nested.exists():
                nested_parent.mkdir(parents=True, exist_ok=True)
                try:
                    nested.symlink_to(cc_static, target_is_directory=True)
                except (OSError, NotImplementedError):
                    logger.warning("could not symlink cc static into %s", work_dir)

            # The repo's config/sounds (hold audio phone-calling.wav, queue
            # prompts, error cues) must ALSO be visible under the work dir:
            # the suite writes its own work-dir config/ files, so without
            # this mirror resolve_audio_file_path's "config/<file>" fallback
            # misses and hold music / error cues degrade to silence.
            repo_config_sounds = project_root / "config" / "sounds"
            if repo_config_sounds.is_dir():
                dst = work_dir / "config" / "sounds"
                if not dst.exists():
                    try:
                        dst.symlink_to(repo_config_sounds, target_is_directory=True)
                    except (OSError, NotImplementedError):
                        logger.warning(
                            "could not symlink config/sounds into %s", work_dir
                        )

        log_dir = self.work_dir / "tests" / "logs"
        self.log_file_path = log_dir / f"rustpbx_regression_{int(time.time())}.log"

        # Refuse to start over a stale listener: a leftover instance holding
        # SIP/HTTP makes the readiness poll succeed against the WRONG process
        # (stale registry/webhook state → baffling cross-test failures).
        # A leftover that belongs to THIS harness (same work-dir config) is
        # SIGKILLed automatically first: rustpbx's graceful drain can hold
        # the port for up to 300 s with active calls, outliving stop()'s
        # 3 s SIGTERM window.
        for port_desc, port in (("SIP", self.sip_port), ("HTTP", self.http_port)):
            culprit = PbxServer._port_holder(port)
            if culprit and "rustpbx" in culprit["cmdline"] and "--conf" in culprit["cmdline"]:
                logger.warning(
                    "killing leftover harness rustpbx PID %s holding %s port %s",
                    culprit["pid"], port_desc, port,
                )
                try:
                    os.kill(culprit["pid"], signal.SIGKILL)
                    time.sleep(1.0)
                except (OSError, ProcessLookupError):
                    pass
                culprit = PbxServer._port_holder(port)
            if culprit:
                raise RuntimeError(
                    f"{port_desc} port {port} is already bound by PID "
                    f"{culprit['pid']} ({culprit['cmdline']}) — a leftover "
                    "instance from an earlier session. Kill it "
                    "(pkill -9 -f 'rustpbx.*--conf') or point this run at "
                    "other ports via RUSTPBX_SIP_PORT / RUSTPBX_HTTP_PORT."
                )

        log_f = open(self.log_file_path, "w", encoding="utf-8")
        cmd = [str(self.binary), "--conf", str(self.config_path)]
        logger.info("Starting rustpbx: %s", " ".join(cmd))
        self.process = subprocess.Popen(
            cmd,
            stdout=log_f,
            stderr=subprocess.STDOUT,
            cwd=str(self.work_dir),
            preexec_fn=os.setsid if os.name != "nt" else None,
        )
        self._spawn_cmd = cmd
        self._spawn_log_f = log_f
        with self._swap_lock:
            self._generation += 1
        gen = self._generation
        self._stopping = False
        self._respawns = 0

        # Supervised restart: `/ami/v1/reload/app` legitimately exits the
        # process (in production an external supervisor restarts it with the
        # reloaded config). Without a watcher, one reload test leaves a dead
        # PBX behind and every later test in the session fails with
        # connection-refused (the 190-setup-error cascade).
        threading.Thread(
            target=self._supervise, args=(cmd, log_f, gen), daemon=True
        ).start()
        logger.info(
            "supervisor watchdog armed (gen=%d) cmd=%s", gen, " ".join(cmd)
        )

        # health check
        if not self._wait_ready(timeout):
            self.dump_logs()
            self._stopping = True
            # The half-started process holds the SIP/HTTP ports — without a
            # hard kill every subsequent run fails with "address already in
            # use" (setsid puts it in its own process group, so the pytest
            # teardown never sees it).
            if self.process is not None:
                try:
                    os.killpg(os.getpgid(self.process.pid), signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
            raise RuntimeError(
                f"rustpbx did not become ready within {timeout}s. "
                f"Check {self.log_file_path}"
            )
        logger.info("rustpbx ready on SIP %s, HTTP %s", self.sip_addr, self.http_url)

    def _wait_ready(self, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.process and self.process.poll() is not None:
                return False
            if self._check_http_health():
                return True
            time.sleep(0.05)
        return False

    def _check_http_health(self) -> bool:
        # Readiness probe. urllib raises HTTPError on 4xx/5xx, so every path
        # here must return <500 when the server is up:
        #   /console/login  — plain console route (no addon assets needed)
        #   /console/cc/dev — needs src/addons/cc/static (symlinked below)
        #   /healthz, /health — observability addon (not in every build)
        for path in ["/console/login", "/console/cc/dev", "/healthz", "/health"]:
            try:
                url = f"{self.http_url}{path}"
                req = urllib.request.Request(url, method="GET")
                with urllib.request.urlopen(req, timeout=3) as resp:
                    if resp.status < 500:
                        return True
            except Exception:
                continue
        return False

    def check_sip_port(self) -> bool:
        """Send a SIP OPTIONS ping via UDP to check SIP readiness."""
        try:
            msg = (
                f"OPTIONS sip:ping@{self.host}:{self.sip_port} SIP/2.0\r\n"
                f"Via: SIP/2.0/UDP {self.host}:9999;branch=z9hG4bK-ping\r\n"
                f"From: <sip:ping@{self.host}>;tag=ping\r\n"
                f"To: <sip:ping@{self.host}>\r\n"
                f"Call-ID: ping@{self.host}\r\n"
                f"CSeq: 1 OPTIONS\r\n"
                f"Content-Length: 0\r\n\r\n"
            )
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.settimeout(2)
            sock.sendto(msg.encode(), (self.host, self.sip_port))
            sock.close()
            return True
        except Exception:
            return False

    def _supervise(self, cmd, log_f, gen: int):
        """Watch the spawned rustpbx (generation *gen*) and respawn it if it
        exits on its own. `/ami/v1/reload/app` legitimately exits the process
        (prod semantics: an external supervisor restarts it with the reloaded
        config) — without this watcher one reload test leaves a dead PBX and
        every later test in the session fails with connection-refused.
        Exits when superseded by a newer generation or when stopping. Caps at
        3 respawns to avoid a crash loop."""
        while True:
            proc = self.process
            if proc is None or self._generation != gen or self._stopping:
                return
            rc = proc.wait()
            if self._stopping or self._generation != gen or self.process is not proc:
                return
            if self._respawns >= 3:
                logger.error(
                    "rustpbx exited (rc=%s) and the respawn budget (3) is "
                    "exhausted — giving up", rc,
                )
                return
            self._respawns += 1
            logger.warning(
                "rustpbx exited on its own (rc=%s) — supervised respawn "
                "%d/3", rc, self._respawns,
            )
            time.sleep(1.0)
            with self._swap_lock:
                if self._stopping or self._generation != gen:
                    return
                self.process = subprocess.Popen(
                    cmd,
                    stdout=log_f,
                    stderr=subprocess.STDOUT,
                    cwd=str(self.work_dir),
                    preexec_fn=os.setsid if os.name != "nt" else None,
                )
            if self._wait_ready(60):
                logger.info(
                    "supervised respawn %d ready on SIP %s, HTTP %s",
                    self._respawns, self.sip_addr, self.http_url,
                )
            else:
                logger.error(
                    "supervised respawn %d did not become ready within 60s",
                    self._respawns,
                )

    def stop(self) -> None:
        self._stopping = True
        if self.process and self.process.poll() is None:
            logger.info("Stopping rustpbx...")
            try:
                os.killpg(os.getpgid(self.process.pid), signal.SIGTERM)
            except Exception:
                self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(os.getpgid(self.process.pid), signal.SIGKILL)
                except Exception:
                    self.process.kill()
        self.process = None

    def dump_logs(self) -> None:
        if self.log_file_path and self.log_file_path.exists():
            content = self.log_file_path.read_text(encoding="utf-8", errors="replace")
            logger.error("=== rustpbx logs (last 3000 chars) ===\n%s", content[-3000:])

    # ---- REST API client ----

    async def api(self) -> "PbxApiClient":
        if self._http_session is None:
            self._http_session = aiohttp.ClientSession()
        return PbxApiClient(self._http_session, self.http_url, self.rwi_token)

    async def close_api(self) -> None:
        if self._http_session:
            await self._http_session.close()
            self._http_session = None


class PbxApiClient:
    """Async REST client for console + CC phone APIs.

    CC domain endpoints use /api prefix and require PhoneAuth JWT.
    AMI endpoints use no prefix.
    Console endpoints use /console prefix.
    """

    def __init__(self, session: aiohttp.ClientSession, base_url: str, rwi_token: str):
        self.session = session
        self.base_url = base_url
        self.rwi_token = rwi_token
        self._csrf_token: Optional[str] = None
        self._cookies: Optional[dict] = None
        self._jwt_token: Optional[str] = None

    async def login_console(self, identifier: str = "admin", password: str = "admin") -> None:
        async with self.session.post(
            f"{self.base_url}/console/login",
            data={"identifier": identifier, "password": password},
            allow_redirects=False,
        ) as resp:
            self._cookies = {c.key: c.value for c in resp.cookies.values()}
        async with self.session.get(
            f"{self.base_url}/console/api/extensions",
            cookies=self._cookies,
        ) as resp:
            for c in resp.cookies.values():
                if self._cookies is None:
                    self._cookies = {}
                self._cookies[c.key] = c.value

    async def ensure_console_auth(
        self, username: str = "e2e-admin", password: str = "e2e-admin-pass"
    ) -> bool:
        """Register the first (superuser) console account then log in.

        On a fresh `sqlite::memory:` DB there are no users, so the first
        registration is allowed regardless of `allow_registration=false`
        (auth.rs registration_policy first_user). The resulting session
        cookie satisfies BOTH `phone_auth_middleware` and `AuthRequired`,
        unlocking all /api/cc/* console + phone-control endpoints. Returns
        True if a usable session cookie was obtained.
        """
        # Try register (idempotent: fails softly if user already exists).
        try:
            async with self.session.post(
                f"{self.base_url}/console/register",
                data={
                    "email": f"{username}@e2e.test",
                    "username": username,
                    "password": password,
                    "confirm_password": password,
                },
                allow_redirects=False,
            ) as resp:
                # 200 = rendered page (maybe error), 302 = success redirect to login
                _ = resp.status
        except Exception:
            pass
        # Login to obtain the session cookie.
        await self.login_console(identifier=username, password=password)
        ok = bool(self._cookies and any("session" in k for k in self._cookies))
        if ok:
            logger.info("console auth established (session cookie obtained)")
        else:
            logger.warning("console auth failed — /api/cc/* console routes will 303/401")
        return ok

    async def _get_headers(self) -> dict:
        headers = {"Content-Type": "application/json"}
        if self.rwi_token:
            headers["Authorization"] = f"Bearer {self.rwi_token}"
        # CSRF guard on /api/* validates x-csrf-token header against the
        # csrf_token cookie (set by login). Echo it for unsafe methods.
        if self._cookies and "csrf_token" in self._cookies:
            headers["x-csrf-token"] = self._cookies["csrf_token"]
        return headers

    async def get(self, path: str, **kw) -> Any:
        headers = await self._get_headers()
        async with self.session.get(
            f"{self.base_url}{path}", headers=headers, cookies=self._cookies, **kw
        ) as resp:
            return await self._handle(resp)

    async def post(self, path: str, json_data: Any = None, **kw) -> Any:
        headers = await self._get_headers()
        # aiohttp forces a Content-Type on every POST (application/json for
        # the json= argument, application/octet-stream otherwise), and the
        # server rejects an EMPTY body advertised as JSON. Endpoints like cc
        # hold/unhold accept no body at all (all-optional fields), so send an
        # empty JSON object instead of an empty body.
        payload = json_data if json_data is not None else {}
        async with self.session.post(
            f"{self.base_url}{path}",
            headers=headers,
            json=payload,
            cookies=self._cookies,
            **kw,
        ) as resp:
            return await self._handle(resp)

    async def put(self, path: str, json_data: Any = None, **kw) -> Any:
        headers = await self._get_headers()
        async with self.session.put(
            f"{self.base_url}{path}",
            headers=headers,
            json=json_data,
            cookies=self._cookies,
            **kw,
        ) as resp:
            return await self._handle(resp)

    async def delete(self, path: str, **kw) -> Any:
        headers = await self._get_headers()
        async with self.session.delete(
            f"{self.base_url}{path}", headers=headers, cookies=self._cookies, **kw
        ) as resp:
            return await self._handle(resp)

    async def patch(self, path: str, json_data: Any = None, **kw) -> Any:
        headers = await self._get_headers()
        async with self.session.patch(
            f"{self.base_url}{path}",
            headers=headers,
            json=json_data,
            cookies=self._cookies,
            **kw,
        ) as resp:
            return await self._handle(resp)

    async def raw_request(self, method: str, path: str, json_data: Any = None) -> tuple[int, Any]:
        """Low-level request returning (status_code, body) without raising on
        4xx/5xx. Use for endpoint-wiring checks where a non-200 is the expected
        outcome (e.g. validation/404 probes)."""
        headers = await self._get_headers()
        async with self.session.request(
            method,
            f"{self.base_url}{path}",
            headers=headers,
            json=json_data,
            cookies=self._cookies,
        ) as resp:
            if resp.content_type == "application/json":
                body = await resp.json()
            else:
                # Non-JSON responses (WAV downloads, text errors, ...) must be
                # returned as raw bytes — decoding e.g. audio/wav as UTF-8
                # raises UnicodeDecodeError in the caller.
                body = await resp.read()
            return resp.status, body

    async def _handle(self, resp: aiohttp.ClientResponse) -> Any:
        if resp.content_type == "application/json":
            data = await resp.json()
        else:
            data = await resp.text()
        if resp.status != 200:
            logger.debug("API %s %s %s: %s", resp.method, resp.status, resp.url, str(data)[:500])
        if resp.status == 401:
            return None
        if resp.status == 404:
            return None
        if resp.status >= 400:
            raise aiohttp.ClientResponseError(
                resp.request_info, resp.history, status=resp.status,
                message=f"API error {resp.status}: {data}",
            )
        return data

    # ---- CC domain helpers (use /api prefix) ----

    async def list_agents(self) -> Any:
        return await self.get("/api/cc/agents")

    async def create_agent(self, data: dict) -> Any:
        return await self.post("/api/cc/agents", data)

    async def get_agent(self, agent_id: str) -> Any:
        return await self.get(f"/api/cc/agents/{agent_id}")

    async def update_agent(self, agent_id: str, data: dict) -> Any:
        return await self.put(f"/api/cc/agents/{agent_id}", data)

    async def delete_agent(self, agent_id: str) -> Any:
        return await self.delete(f"/api/cc/agents/{agent_id}")

    async def update_agent_status(self, agent_id: str, status: str) -> Any:
        return await self.post(f"/api/cc/agents/{agent_id}/status", {"status": status})

    async def end_agent_wrapup(self, agent_id: str) -> Any:
        return await self.post(f"/api/cc/agents/{agent_id}/wrapup/end", {})

    async def list_skill_groups(self) -> Any:
        return await self.get("/api/cc/skill-groups")

    async def create_skill_group(self, data: dict) -> Any:
        return await self.post("/api/cc/skill-groups", data)

    async def list_queues(self) -> Any:
        return await self.get("/api/cc/queues")

    async def list_acd_policies(self) -> Any:
        return await self.get("/api/cc/acd/policies")

    async def originate(self, data: dict) -> Any:
        return await self.post("/api/cc/calls/originate", data)

    async def list_active_calls(self) -> Any:
        return await self.get("/api/cc/calls/active")

    async def end_call(self, call_id: str) -> Any:
        return await self.post(f"/api/cc/calls/{call_id}/end")

    async def hold_call(self, call_id: str) -> Any:
        return await self.post(f"/api/cc/calls/{call_id}/hold")

    async def unhold_call(self, call_id: str) -> Any:
        return await self.post(f"/api/cc/calls/{call_id}/unhold")

    async def blind_transfer(self, call_id: str, target: str) -> Any:
        return await self.post(f"/api/cc/calls/{call_id}/transfer", {"target": target})

    async def send_dtmf(self, call_id: str, digits: str) -> Any:
        return await self.post(f"/api/cc/calls/{call_id}/send-dtmf", {"digits": digits})

    async def submit_acw(self, call_id: str, data: Optional[dict] = None) -> Any:
        return await self.post(f"/api/cc/calls/{call_id}/acw", data or {})

    async def add_call_note(self, call_id: str, note: str) -> Any:
        return await self.patch(f"/api/cc/calls/{call_id}", {"note": note})

    async def get_phone_config(self) -> Any:
        return await self.get("/api/cc/phone/config")

    async def get_realtime_stats(self) -> Any:
        return await self.get("/api/cc/realtime")

    async def get_dashboard_summary(self) -> Any:
        return await self.get("/api/cc/dashboard/summary")

    async def get_agent_breaks(self, agent_id: str) -> Any:
        return await self.get(f"/api/cc/agents/{agent_id}/breaks")

    async def reload_agents(self) -> Any:
        return await self.post("/api/cc/agents/reload")

    async def reload_skill_groups(self) -> Any:
        return await self.post("/api/cc/skill-groups/reload")

    async def reload_acd(self) -> Any:
        return await self.post("/api/cc/acd/reload")

    async def reload_trunks(self) -> Any:
        return await self.post("/ami/v1/reload/trunks")

    async def reload_routes(self) -> Any:
        return await self.post("/ami/v1/reload/routes")

    async def reload_trunks(self) -> Any:
        return await self.post("/ami/v1/reload/trunks")

    async def reload_queues(self) -> Any:
        return await self.post("/ami/v1/reload/queues")

    async def reload_acl(self) -> Any:
        return await self.post("/ami/v1/reload/acl")

    async def reload_app(self) -> Any:
        return await self.post("/ami/v1/reload/app")

    async def get_health(self) -> Any:
        return await self.get("/healthz")

    async def get_metrics(self) -> str:
        async with self.session.get(f"{self.base_url}/metrics") as resp:
            return await resp.text()

    # ---- CC data seeding (agents/skill-groups via REST) ----
    # NOTE: the CC addon does NOT load agents from agents_files; agents must be
    # created via POST /api/cc/agents. This seeds the default regression set so
    # agent/queue/skill-group dependent tests have data to work against.

    async def seed_default_agents(self) -> dict:
        """Create the default regression agents (1001/1002/1003) + skill-groups.

        Idempotent: 409/400 on duplicates is treated as success. Returns a
        summary dict. Requires console auth (ensure_console_auth first).
        """
        agents = [
            {"agent_id": "1001", "display_name": "Agent 1001 (Regression)",
             "skills": ["support", "sales"], "max_concurrency": 3, "role": "agent"},
            {"agent_id": "1002", "display_name": "Agent 1002 (Regression)",
             "skills": ["support"], "max_concurrency": 3, "role": "agent"},
            {"agent_id": "1003", "display_name": "Agent 1003 (Regression)",
             "skills": ["support", "sales", "vip"], "max_concurrency": 3, "role": "agent"},
        ]
        skill_groups = [
            {"skill_group_id": "support", "skills_required": ["support"],
             "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 90},
            {"skill_group_id": "sales", "skills_required": ["sales"],
             "overflow_groups": [], "sla_target_secs": 30, "max_wait_secs": 90},
            {"skill_group_id": "vip", "skills_required": ["vip"],
             "overflow_groups": ["support"], "sla_target_secs": 15, "max_wait_secs": 60},
        ]
        summary = {"agents": 0, "skill_groups": 0, "errors": []}
        for a in agents:
            try:
                await self.post("/api/cc/agents", a)
                summary["agents"] += 1
            except Exception as exc:  # 409/400 duplicate → not an error
                if "409" in str(exc) or "400" in str(exc) or "already" in str(exc).lower():
                    summary["agents"] += 1
                else:
                    summary["errors"].append(f"agent {a['agent_id']}: {exc}")
        for sg in skill_groups:
            try:
                await self.post("/api/cc/skill-groups", sg)
                summary["skill_groups"] += 1
            except Exception as exc:
                if "409" in str(exc) or "400" in str(exc) or "already" in str(exc).lower():
                    summary["skill_groups"] += 1
                else:
                    summary["errors"].append(f"skill_group {sg['skill_group_id']}: {exc}")
        logger.info("seed_default_agents: %s", summary)
        return summary
