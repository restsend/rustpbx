"""Dynamic TOML config generation for RustPBX E2E tests.

Builds a complete `rustpbx.toml` with webhook, routes, trunks, queues, IVR,
agents, skill groups, and ACD policies wired to test infrastructure.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Optional


def _toml_escape(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"')


def _toml_value(val: Any) -> str:
    if isinstance(val, bool):
        return "true" if val else "false"
    if isinstance(val, (int, float)):
        return str(val)
    if isinstance(val, str):
        return f'"{_toml_escape(val)}"'
    if isinstance(val, list):
        return "[" + ", ".join(_toml_value(v) for v in val) + "]"
    return _toml_escape(str(val))


class ConfigBuilder:
    """Fluent builder that writes a complete rustpbx TOML config."""

    def __init__(
        self,
        *,
        project_root: Path,
        work_dir: Path,
        sip_port: int = 5060,
        http_port: int = 8080,
        webhook_url: str = "",
        rwi_token: str = "test-api-key-e2e",
        database_url: str = "sqlite::memory:",
        addons: Optional[list[str]] = None,
    ):
        self.project_root = project_root
        self.work_dir = work_dir
        self.sip_port = sip_port
        self.http_port = http_port
        self.webhook_url = webhook_url
        self.rwi_token = rwi_token
        self.database_url = database_url
        # "telemetry" mounts the /metrics + /healthz routers (addon id under
        # the contact-center build; the community build uses "observability").
        # Without it the observability acceptance tests (L020/L297/L298/L300)
        # get empty bodies.
        self.addons = addons or ["cc", "telemetry"]
        self.licenses: dict[str, str] = {}
        self.trunks: dict[str, dict] = {}
        self.routes: list[dict] = {}
        self.queues: dict[str, dict] = {}
        self.extra_memory_users: list[str] = []
        self.extra_guest_users: list[str] = []
        self.ivr_files: dict[str, str] = {}  # route_point -> TOML body
        self.agents_file: Optional[str] = None
        self.skill_groups_file: Optional[str] = None
        self.acd_file: Optional[str] = None
        self.http_router: Optional[dict] = None
        self.voicemail_config: Optional[dict] = None
        self.sbc_jsonrpc_config: Optional[dict] = None
        self.sipflow_engine: str = "flowdb"
        self.sipflow_root: str = "./config/sipflow"
        self.recording_force_file: bool = False
        self.users_support_webrtc: bool = False  # RTP-first; WebRTC opt-in per test
        self.webrtc_usernames: set[str] = set()  # specific users with WebRTC enabled
        self.media_proxy: Optional[str] = None  # e.g. "all" for bridge/anchored media
        self.media_config: Optional[dict] = None  # [media] section (comfort_noise, etc.)
        self.realms: Optional[list[str]] = None  # [proxy] realms — local SIP realms
        # [proxy] parallel_fork — when a callee has multiple registered devices,
        # ring ALL of them in parallel (default true). None = omit (PBX default).
        self.parallel_fork: Optional[bool] = None
        # [proxy] max_ring_time — global default max ring time (seconds) before
        # a no-answer call is rejected with 408; 0 = disabled (ring forever).
        # None = omit (PBX default = disabled).
        self.max_ring_time: Optional[int] = None
        self.config_name = "rustpbx_regression.toml"
        self.log_level = os.environ.get("RUSTPBX_E2E_LOG_LEVEL", "info")
        # Outbound dial SSE interface ([outbound] section).
        self.outbound_enabled: bool = False
        # [proxy.ivr_fallback] — Step IVR session recovery (rules + default).
        self.ivr_fallback: Optional[dict] = None
        # Per-user call forwarding overrides: username → dict(
        # mode="always"|"when_busy"|"no_answer", destination=str, timeout=int)
        self.user_forwarding: dict[str, dict] = {}
        # [proxy] session timer (RFC 4028): None = omit (PBX default off).
        # Note the PBX enforces Min-SE=90s (smaller session_expires is bumped
        # to 90; inbound offers below it are rejected with 422).
        self.session_timer: Optional[bool] = None
        self.session_timer_always: Optional[bool] = None
        self.session_expires: Optional[int] = None
        # [proxy.emergency] — {enabled, numbers, trunk_uri}
        self.emergency: Optional[dict] = None
        # [proxy] modules override (default: acl, auth, registrar, call).
        # Add "presence" for SUBSCRIBE/MWI tests, etc.
        self.proxy_modules: Optional[list[str]] = None
        # [recording] auto_start — default True (matches the suite). Disable
        # for tests whose app issues its own record commands (voicemail
        # deposit collides with the auto recorder: "recording_already_active").
        self.recording_auto_start: bool = True

    # ---- addons / licenses ----

    def set_addons(self, addons: list[str]) -> "ConfigBuilder":
        self.addons = list(addons)
        return self

    def set_license(self, addon: str, license_key: str = "global") -> "ConfigBuilder":
        self.licenses[addon] = license_key
        return self

    # ---- feature blocks ----

    def enable_http_router(
        self,
        url: str,
        *,
        headers: Optional[dict] = None,
        timeout_ms: int = 5000,
        fallback_to_static: bool = True,
    ) -> "ConfigBuilder":
        self.http_router = {
            "url": url,
            "headers": headers or {},
            "timeout_ms": timeout_ms,
            "fallback_to_static": fallback_to_static,
        }
        return self

    def add_voicemail(
        self,
        *,
        spool_dir: Optional[str] = None,
        storage_path: Optional[str] = None,
        max_duration_secs: int = 300,
        check_voicemail_number: str = "*97",
        check_voicemail: Optional[dict] = None,
    ) -> "ConfigBuilder":
        self.voicemail_config = {
            "spool_dir": spool_dir or "./config/voicemail/spool",
            "storage_path": storage_path or "./config/voicemail/recordings",
            "max_duration_secs": max_duration_secs,
            "check_voicemail_number": check_voicemail_number,
            "check_voicemail": check_voicemail,
        }
        self.set_license("voicemail")
        if "voicemail" not in self.addons:
            self.addons.append("voicemail")
        return self

    def add_sbc_jsonrpc(
        self,
        rules: list[dict],
        *,
        enabled: bool = True,
        timeout_ms: int = 5000,
    ) -> "ConfigBuilder":
        self.sbc_jsonrpc_config = {
            "enabled": enabled,
            "timeout_ms": timeout_ms,
            "rules": rules,
        }
        self.set_license("sbc")
        if "sbc" not in self.addons:
            self.addons.append("sbc")
        return self

    def set_sipflow(
        self,
        *,
        engine: str = "flowdb",
        root: Optional[str] = None,
        subdirs: str = "daily",
    ) -> "ConfigBuilder":
        self.sipflow_engine = engine
        self.sipflow_root = root or self.sipflow_root
        return self

    def set_wholesale(self) -> "ConfigBuilder":
        self.set_license("wholesale")
        if "wholesale" not in self.addons:
            self.addons.append("wholesale")
        return self

    def set_recording_force_file(self, force_file: bool = True) -> "ConfigBuilder":
        """Allow live RWI record_start/stop alongside an enabled SipFlow backend."""
        self.recording_force_file = force_file
        return self

    def set_users_support_webrtc(self, enabled: bool = True) -> "ConfigBuilder":
        """Register users as WebRTC-capable (DTLS-SRTP). Default is RTP-only.

        rustpbx supports both RTP and WebRTC media; leaving users RTP-only makes
        plain sipbot RTP calls work without DTLS. Enable only for tests that
        exercise WebRTC legs.
        """
        self.users_support_webrtc = enabled
        return self

    def set_webrtc_users(self, usernames: list[str]) -> "ConfigBuilder":
        """Enable WebRTC for specific users only (others stay RTP).

        Use this instead of set_users_support_webrtc when only some users
        are WebRTC-capable (e.g. WebRTC caller → RTP callee).
        """
        self.webrtc_usernames = set(usernames)
        return self

    def set_media(
        self, comfort_noise: bool = True, level_db: float = -35.0
    ) -> "ConfigBuilder":
        """Configure the [media] section (comfort noise settings).

        When comfort_noise=true, the PBX emits low-level noise instead of
        digital silence during gaps (hold, bridge transitions, no source).
        """
        self.media_config = {"comfort_noise": comfort_noise, "level_db": level_db}
        return self

    def set_realms(self, realms: list[str]) -> "ConfigBuilder":
        """Set the PBX's local SIP realms ([proxy].realms).

        With realms explicitly configured, domains outside this list are treated
        as *external* (e.g. an inbound trunk's From domain), which makes the
        call classify as Inbound — the classification that applies per-trunk
        media config such as ringback/busy tones.
        """
        self.realms = realms
        return self

    def set_parallel_fork(self, enabled: bool) -> "ConfigBuilder":
        """Control [proxy] parallel_fork (P2P calls ring all registered devices).

        Default is True (PBX rings every binding in parallel). Set to False to
        restore the legacy "last registered device only" behavior.
        """
        self.parallel_fork = enabled
        return self

    def set_max_ring_time(self, secs: int) -> "ConfigBuilder":
        """Set [proxy] max_ring_time (global default, seconds).

        0 disables the ring timeout (calls ring until answered or cancelled).
        """
        self.max_ring_time = int(secs)
        return self

    def set_user_forwarding(
        self,
        username: str,
        mode: str,
        destination: str,
        timeout_secs: Optional[int] = None,
    ) -> "ConfigBuilder":
        """Configure per-user call forwarding.

        mode: "always" | "when_busy" | "no_answer" (see SipUser::forwarding_config).
        destination: a TransferEndpoint string (e.g. "1003" or "sip:1003@host").
        """
        self.user_forwarding[username] = {
            "mode": mode,
            "destination": destination,
            "timeout": timeout_secs,
        }
        return self

    def set_session_timer(
        self,
        enabled: bool = True,
        always: bool = False,
        expires_secs: Optional[int] = None,
    ) -> "ConfigBuilder":
        """Enable [proxy] session_timer (RFC 4028) refreshes."""
        self.session_timer = enabled
        self.session_timer_always = always
        self.session_expires = expires_secs
        return self

    def set_emergency(
        self, trunk_uri: str, numbers: Optional[list[str]] = None
    ) -> "ConfigBuilder":
        """Enable [proxy.emergency] routing to a dedicated trunk URI."""
        self.emergency = {
            "trunk_uri": trunk_uri,
            "numbers": numbers or ["110", "119", "120", "122", "911", "999"],
        }
        return self

    def _user_forwarding_lines(self, username: str) -> list[str]:
        fwd = self.user_forwarding.get(username)
        if not fwd:
            return []
        lines = [f'call_forwarding_mode = "{fwd["mode"]}"']
        lines.append(f'call_forwarding_destination = "{fwd["destination"]}"')
        if fwd.get("timeout") is not None:
            lines.append(f"call_forwarding_timeout = {fwd['timeout']}")
        return lines

    # ---- builders ----

    def add_trunk(
        self,
        name: str,
        *,
        dest: str,
        direction: str = "bidirectional",
        transport: str = "udp",
        trunk_id: Optional[int] = None,
        register_enabled: bool = False,
        register_expires: Optional[int] = None,
        max_calls: Optional[int] = None,
        max_cps: Optional[int] = None,
        username: Optional[str] = None,
        password: Optional[str] = None,
        inbound_hosts: Optional[list[str]] = None,
        rewrite_hostport: bool = True,
        call_id_mode: Optional[str] = None,
        backup_dest: Optional[str] = None,
        header_rules: Optional[list[dict]] = None,
        ringback: Optional[dict] = None,
        max_ring_time: Optional[int] = None,
    ) -> "ConfigBuilder":
        t: dict[str, Any] = {
            "dest": dest,
            "transport": transport,
            "direction": direction,
            "rewrite_hostport": rewrite_hostport,
        }
        if trunk_id is not None:
            t["id"] = trunk_id
        if register_enabled:
            t["register_enabled"] = True
        if register_expires is not None:
            t["register_expires"] = register_expires
        if max_calls is not None:
            t["max_calls"] = max_calls
        if max_cps is not None:
            t["max_cps"] = max_cps
        if username:
            t["username"] = username
        if password:
            t["password"] = password
        if inbound_hosts:
            t["inbound_hosts"] = inbound_hosts
        if call_id_mode:
            t["call_id_mode"] = call_id_mode
        if backup_dest:
            t["backup_dest"] = backup_dest
        if header_rules:
            t["header_rules"] = header_rules
        if ringback is not None:
            t["ringback"] = ringback
        if max_ring_time is not None:
            t["max_ring_time"] = max_ring_time
        self.trunks[name] = t
        return self

    def add_route(
        self,
        name: str,
        *,
        match: dict,
        priority: int = 100,
        action: str = "forward",
        dest: Optional[str | list[str]] = None,
        select: str = "rr",
        queue: Optional[str] = None,
        app: Optional[str] = None,
        app_params: Optional[dict] = None,
        source_trunks: Optional[list[str]] = None,
        source_trunk_ids: Optional[list[int]] = None,
        hash_key: Optional[str] = None,
        rewrite: Optional[dict] = None,
        reject_code: Optional[int] = None,
        reject_reason: Optional[str] = None,
        auto_answer: bool = True,
        disabled: bool = False,
        max_ring_time: Optional[int] = None,
    ) -> "ConfigBuilder":
        r: dict[str, Any] = {
            "name": name,
            "priority": priority,
            "match": match,
            "action": action,
            "select": select,
            "auto_answer": auto_answer,
            "disabled": disabled,
        }
        if dest:
            r["dest"] = dest
        if queue:
            r["queue"] = queue
        if app:
            r["app"] = app
        if app_params:
            r["app_params"] = app_params
        if source_trunks:
            r["source_trunks"] = source_trunks
        if source_trunk_ids:
            r["source_trunk_ids"] = source_trunk_ids
        if hash_key:
            r["hash_key"] = hash_key
        if rewrite:
            r["rewrite"] = rewrite
        if reject_code:
            r["reject"] = {"code": reject_code}
            if reject_reason:
                r["reject"]["reason"] = reject_reason
        if max_ring_time is not None:
            r["max_ring_time"] = max_ring_time
        self.routes[name] = r
        return self

    def add_memory_users(self, usernames: list[str]) -> "ConfigBuilder":
        """Register additional SIP memory users (password 123456, plain RTP).

        Needed so checklist agents (2xxx) can REGISTER with sipbot — the
        registrar/auth backend only knows users listed here, not CC agent rows.
        """
        self.extra_memory_users.extend(usernames)
        return self

    def add_guest_user(self, usernames: list[str]) -> "ConfigBuilder":
        """Register memory users with ``allow_guest_calls = true``.

        Unauthenticated callers (e.g. RWI ``call.originate`` legs, which carry
        no SIP credentials) can only reach users with the guest flag — queue /
        IVR entry numbers in production are the same idea: dialable without
        registration.
        """
        self.extra_guest_users.extend(usernames)
        return self

    def add_queue(
        self,
        name: str,
        *,
        strategy_mode: str = "sequential",
        targets: Optional[list[str]] = None,
        accept_immediately: bool = False,
        passthrough_ringback: bool = False,
        hold_audio: Optional[str] = None,
        loop_playback: bool = True,
        wait_timeout_secs: Optional[int] = None,
        ring_timeout_secs: Optional[int] = None,
        fallback_redirect: Optional[str] = None,
        fallback_queue: Optional[str] = None,
        fallback_failure_code: Optional[int] = None,
        skill_group_ref: Optional[str] = None,
        voice_prompts: Optional[dict[str, str]] = None,
    ) -> "ConfigBuilder":
        q: dict[str, Any] = {
            "name": name,
            "accept_immediately": accept_immediately,
            "passthrough_ringback": passthrough_ringback,
            "strategy": {
                "mode": strategy_mode,
                "targets": [
                    {"uri": t, "label": t.split("@")[0] if "@" in t else t}
                    for t in (targets or [])
                ],
            },
        }
        if ring_timeout_secs is not None:
            q["ring_timeout_secs"] = ring_timeout_secs
        if hold_audio or loop_playback:
            q["hold"] = {
                "audio_file": hold_audio or "sounds/phone-calling.wav",
                "loop_playback": loop_playback,
            }
        if wait_timeout_secs is not None:
            q["strategy"]["wait_timeout_secs"] = wait_timeout_secs
        fb: dict = {}
        if fallback_redirect:
            fb["redirect"] = fallback_redirect
        if fallback_queue:
            fb["queue_ref"] = fallback_queue
        if fallback_failure_code:
            fb["failure_code"] = fallback_failure_code
        if skill_group_ref:
            fb["skill_group_ref"] = skill_group_ref
        if fb:
            q["fallback"] = fb
        if voice_prompts:
            q["voice_prompts"] = dict(voice_prompts)
        self.queues[name] = q
        return self

    def add_ivr(
        self,
        route_point: str,
        toml_body: str,
    ) -> "ConfigBuilder":
        self.ivr_files[route_point] = toml_body
        return self

    def set_ivr_fallback(
        self,
        *,
        default: Optional[str] = None,
        rules: Optional[list[dict]] = None,
    ) -> "ConfigBuilder":
        """Configure ``[proxy.ivr_fallback]`` for Step IVR recovery.

        Each rule is ``{"name"?, "priority"?, "match": {"from.user": "..."}, "target": "..."}``.
        """
        self.ivr_fallback = {
            "default": default,
            "rules": list(rules or []),
        }
        return self

    def set_agents_file(self, path: str) -> "ConfigBuilder":
        self.agents_file = path
        return self

    def set_skill_groups_file(self, path: str) -> "ConfigBuilder":
        self.skill_groups_file = path
        return self

    def set_acd_file(self, path: str) -> "ConfigBuilder":
        self.acd_file = path
        return self

    # ---- rendering ----

    def _write_trunk_toml(self) -> str:
        lines: list[str] = ["[trunks]"]
        for name, t in self.trunks.items():
            lines.append("")
            lines.append(f"[trunks.{name}]")
            nested: list[tuple[str, Any]] = []
            for k, v in t.items():
                if k == "ringback" and isinstance(v, dict):
                    nested.append((k, v))
                elif k == "header_rules" and isinstance(v, list):
                    nested.append((k, v))
                elif isinstance(v, list):
                    lines.append(f"{k} = {_toml_value(v)}")
                else:
                    lines.append(f"{k} = {_toml_value(v)}")
            for k, v in nested:
                if k == "ringback" and isinstance(v, dict):
                    lines.append("")
                    lines.append(f"[trunks.{name}.ringback]")
                    for rk, rv in v.items():
                        lines.append(f"{rk} = {_toml_value(rv)}")
                elif k == "header_rules" and isinstance(v, list):
                    lines.extend(self._render_header_rules(v))
        return "\n".join(lines)

    def _render_header_rules(self, rules: list[dict]) -> list[str]:
        out: list[str] = []
        for i, rule in enumerate(rules):
            out.append("")
            out.append(f"[[header_rules]]")
            out.append(f'action = "{rule.get("action", "add")}"')
            out.append(f'name = "{rule["name"]}"')
            if "value" in rule:
                out.append(f'value = "{rule["value"]}"')
            if "match_caller_prefix" in rule:
                out.append(f'match_caller_prefix = "{rule["match_caller_prefix"]}"')
            if "match_callee_prefix" in rule:
                out.append(f'match_callee_prefix = "{rule["match_callee_prefix"]}"')
        return out

    def _write_routes_toml(self) -> str:
        lines: list[str] = []
        for idx, (name, r) in enumerate(self.routes.items()):
            lines.append("")
            lines.append("[[routes]]")
            lines.append(f'name = "{r["name"]}"')
            lines.append(f'priority = {r["priority"]}')
            lines.append(f'select = "{r["select"]}"')
            lines.append(f'auto_answer = {"true" if r["auto_answer"] else "false"}')
            lines.append(f'disabled = {"true" if r["disabled"] else "false"}')
            if r.get("max_ring_time") is not None:
                lines.append(f'max_ring_time = {r["max_ring_time"]}')
            action = r["action"]
            if action == "forward":
                if r.get("dest"):
                    dest = r["dest"]
                    if isinstance(dest, list):
                        lines.append(f'dest = {_toml_value(dest)}')
                    else:
                        lines.append(f'dest = "{dest}"')
            elif action == "queue":
                lines.append('action = "queue"')
                if r.get("queue"):
                    lines.append(f'queue = "{r["queue"]}"')
            elif action == "application":
                lines.append(f'app = "{r.get("app", "ivr")}"')
                if r.get("app_params"):
                    lines.append("")
                    lines.append("[routes.app_params]")
                    for k, v in r["app_params"].items():
                        lines.append(f'{k} = "{v}"')
            elif action == "reject":
                lines.append('action = "reject"')
                if r.get("reject"):
                    lines.append('[reject]')
                    lines.append(f'code = {r["reject"]["code"]}')
                    if r["reject"].get("reason"):
                        lines.append(f'reason = "{r["reject"]["reason"]}"')
            elif action == "busy":
                lines.append('action = "busy"')
            # source_trunks
            if r.get("source_trunks"):
                lines.append(f"source_trunks = {_toml_value(r['source_trunks'])}")
            if r.get("source_trunk_ids"):
                lines.append(f"source_trunk_ids = {_toml_value(r['source_trunk_ids'])}")
            # match block
            lines.append("")
            lines.append("[routes.match]")
            for k, v in r["match"].items():
                lines.append(f'"{k}" = "{v}"')
            if r.get("rewrite"):
                lines.append("")
                lines.append("[routes.rewrite]")
                for k, v in r["rewrite"].items():
                    lines.append(f'"{k}" = "{v}"')
        return "\n".join(lines)

    def _render_app_params(self, params: dict) -> str:
        parts = []
        for k, v in params.items():
            parts.append(f'{k} = "{v}"')
        return "{ " + ", ".join(parts) + " }"

    def _write_queue_files(self) -> list[Path]:
        """Write one `QueueFileDocument` per queue: `config/queue/e2e_<name>.toml`.

        rustpbx parses each queue include file as a single
        `QueueFileDocument { name, queue: RouteQueueConfig }`, so the queue
        config lives under a `[queue]` table.
        """
        if not self.queues:
            return []
        queue_dir = self.work_dir / "config" / "queue"
        queue_dir.mkdir(parents=True, exist_ok=True)
        for stale in queue_dir.glob("*.toml"):
            stale.unlink()
        written: list[Path] = []
        for name, q in self.queues.items():
            lines = [f'name = "{q["name"]}"', "", "[queue]"]
            lines.append(
                f'accept_immediately = {"true" if q["accept_immediately"] else "false"}'
            )
            lines.append(
                f'passthrough_ringback = {"true" if q["passthrough_ringback"] else "false"}'
            )
            if q.get("hold"):
                lines.append("")
                lines.append("[queue.hold]")
                lines.append(f'audio_file = "{q["hold"]["audio_file"]}"')
                lines.append(
                    f'loop_playback = {"true" if q["hold"]["loop_playback"] else "false"}'
                )
            if q.get("fallback"):
                lines.append("")
                lines.append("[queue.fallback]")
                for k, v in q["fallback"].items():
                    lines.append(f'{k} = "{v}"' if isinstance(v, str) else f"{k} = {v}")
            lines.append("")
            lines.append("[queue.strategy]")
            lines.append(f'mode = "{q["strategy"]["mode"]}"')
            if q["strategy"].get("wait_timeout_secs"):
                lines.append(f'wait_timeout_secs = {q["strategy"]["wait_timeout_secs"]}')
            if q["strategy"].get("targets"):
                for t in q["strategy"]["targets"]:
                    lines.append("")
                    lines.append("[[queue.strategy.targets]]")
                    lines.append(f'uri = "{t["uri"]}"')
                    if t.get("label"):
                        lines.append(f'label = "{t["label"]}"')
            fp = queue_dir / f"e2e_{name}.toml"
            fp.write_text("\n".join(lines) + "\n", encoding="utf-8")
            written.append(fp)
        return written

    def _write_single_queue_toml(self, name: str, q: dict) -> str:
        """Write ONE queue as flat TOML (QueueFileDocument format).

        QueueFileDocument = { name: String, queue: RouteQueueConfig }.
        So `name` is top-level, and all RouteQueueConfig fields go under [queue].
        """
        lines: list[str] = [
            f'name = "{q["name"]}"',
            "",
            "[queue]",
            f'accept_immediately = {"true" if q["accept_immediately"] else "false"}',
            f'passthrough_ringback = {"true" if q["passthrough_ringback"] else "false"}',
        ]
        if q.get("ring_timeout_secs") is not None:
            lines.append(f'ring_timeout_secs = {q["ring_timeout_secs"]}')
        if q.get("hold"):
            lines.append("")
            lines.append("[queue.hold]")
            lines.append(f'audio_file = "{q["hold"]["audio_file"]}"')
            lines.append(f'loop_playback = {"true" if q["hold"]["loop_playback"] else "false"}')
        if q.get("fallback"):
            lines.append("")
            lines.append("[queue.fallback]")
            for k, v in q["fallback"].items():
                lines.append(f'{k} = "{v}"' if isinstance(v, str) else f"{k} = {v}")
        if q.get("voice_prompts"):
            vp = q["voice_prompts"]
            comfort = vp.pop("comfort_prompts", None)
            if any(v for v in vp.values() if v):
                lines.append("")
                lines.append("[queue.voice_prompts]")
                for k, v in vp.items():
                    if isinstance(v, str) and v:
                        lines.append(f'{k} = "{v}"')
            if comfort:
                for cp in comfort:
                    lines.append("")
                    lines.append("[[queue.voice_prompts.comfort_prompts]]")
                    lines.append(f'audio_file = "{cp["audio_file"]}"')
                    lines.append(f'interval_secs = {cp.get("interval_secs", 30)}')
        lines.append("")
        lines.append("[queue.strategy]")
        lines.append(f'mode = "{q["strategy"]["mode"]}"')
        if q["strategy"].get("wait_timeout_secs"):
            lines.append(f'wait_timeout_secs = {q["strategy"]["wait_timeout_secs"]}')
        if q["strategy"].get("targets"):
            for t in q["strategy"]["targets"]:
                lines.append("")
                lines.append("[[queue.strategy.targets]]")
                lines.append(f'uri = "{t["uri"]}"')
                if t.get("label"):
                    lines.append(f'label = "{t["label"]}"')
        return "\n".join(lines) + "\n"

    def _write_ivr_files(self) -> list[Path]:
        written: list[Path] = []
        ivr_dir = self.work_dir / "config" / "ivr"
        ivr_dir.mkdir(parents=True, exist_ok=True)
        for route_point, body in self.ivr_files.items():
            fp = ivr_dir / f"{route_point}.toml"
            fp.write_text(body, encoding="utf-8")
            written.append(fp)
        return written

    def _write_sbc_jsonrpc(self) -> Optional[Path]:
        if not self.sbc_jsonrpc_config:
            return None
        sbc_dir = self.work_dir / "config" / "sbc"
        sbc_dir.mkdir(parents=True, exist_ok=True)
        fp = sbc_dir / "sbc_jsonrpc.toml"
        lines = [
            f"enabled = {'true' if self.sbc_jsonrpc_config['enabled'] else 'false'}",
            f"timeout_ms = {self.sbc_jsonrpc_config['timeout_ms']}",
        ]
        for rule in self.sbc_jsonrpc_config["rules"]:
            lines.append("")
            lines.append("[[rules]]")
            lines.append(f'name = "{rule["name"]}"')
            lines.append(f'enabled = {"true" if rule.get("enabled", True) else "false"}')
            if rule.get("when"):
                lines.append(f'when = "{rule["when"]}"')
            mg = rule.get("match_group")
            if mg:
                lines.append("")
                lines.append("[rules.match_group]")
                lines.append(f'logic = "{mg.get("logic", "all")}"')
                for cond in mg.get("conditions", []):
                    lines.append("")
                    lines.append("[[rules.match_group.conditions]]")
                    lines.append(f'field = "{cond["field"]}"')
                    lines.append(f'op = "{cond["op"]}"')
                    lines.append(f'value = "{cond["value"]}"')
            up = rule.get("upstream", {})
            lines.append("")
            lines.append("[rules.upstream]")
            lines.append(f'method = "{up.get("method", "POST")}"')
            lines.append(f'url = "{up["url"]}"')
            if up.get("body"):
                lines.append(f"body = {_toml_value(up['body'])}")
            resp = rule.get("response", {})
            lines.append("")
            lines.append("[rules.response]")
            lines.append(f'success_when = {_toml_value(resp.get("success_when", "true"))}')
            lines.append(f'callee_rewrite = {_toml_value(resp.get("callee_rewrite", ""))}')
            lines.append(f'caller_rewrite = {_toml_value(resp.get("caller_rewrite", ""))}')
            lines.append(f'reject_status = {resp.get("reject_status", 403)}')
            lines.append(
                f'reject_on_eval_error = {"true" if resp.get("reject_on_eval_error", True) else "false"}'
            )
            if resp.get("reject_reason"):
                lines.append(f'reject_reason = {_toml_value(resp["reject_reason"])}')
            lines.append(
                f'passthrough_original_headers = {"true" if resp.get("passthrough_original_headers", True) else "false"}'
            )
            if resp.get("inject_headers"):
                for h in resp["inject_headers"]:
                    lines.append("")
                    lines.append("[[rules.response.inject_headers]]")
                    lines.append(f'action = "{h.get("action", "add")}"')
                    lines.append(f'name = "{h["name"]}"')
                    lines.append(f'value = "{h.get("value", "")}"')
            if resp.get("allow_codecs"):
                lines.append(f'allow_codecs = {_toml_value(resp["allow_codecs"])}')
        fp.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return fp

    def _write_voicemail_toml(self) -> Optional[Path]:
        if not self.voicemail_config:
            return None
        v = self.voicemail_config
        lines = [
            f'spool_dir = "{v["spool_dir"]}"',
            f"max_duration_secs = {v['max_duration_secs']}",
            f'check_voicemail_number = "{v["check_voicemail_number"]}"',
            "",
            "[storage]",
            'type = "local"',
            f'path = "{v["storage_path"]}"',
        ]
        cv = v.get("check_voicemail")
        if cv:
            lines.append("")
            lines.append("[check_voicemail]")
            for key, value in cv.items():
                if isinstance(value, bool):
                    lines.append(f"{key} = {'true' if value else 'false'}")
                elif isinstance(value, (int, float)):
                    lines.append(f"{key} = {value}")
                else:
                    lines.append(f'{key} = "{value}"')
        vm_dir = self.work_dir / "config"
        vm_dir.mkdir(parents=True, exist_ok=True)
        fp = vm_dir / "voicemail.toml"
        fp.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return fp

    def build(self) -> Path:
        # write IVR files
        self._write_ivr_files()
        # write trunks file
        trunks_dir = self.work_dir / "config" / "trunks"
        trunks_dir.mkdir(parents=True, exist_ok=True)
        trunks_file = trunks_dir / "e2e_trunks.toml"
        trunks_file.write_text(self._write_trunk_toml(), encoding="utf-8")
        # write routes file
        routes_dir = self.work_dir / "config" / "routes"
        routes_dir.mkdir(parents=True, exist_ok=True)
        routes_file = routes_dir / "e2e_routes.toml"
        routes_file.write_text(self._write_routes_toml(), encoding="utf-8")
        # write queues — one file PER queue (load_queues_from_files parses each
        # file as a single QueueFileDocument, not a HashMap, so all-in-one only
        # loads the first section).
        queues_dir = self.work_dir / "config" / "queue"
        queues_dir.mkdir(parents=True, exist_ok=True)
        if self.queues:
            for name, q in self.queues.items():
                qf = queues_dir / f"e2e_{name}.toml"
                qf.write_text(self._write_single_queue_toml(name, q), encoding="utf-8")

        # write SBC JSON-RPC and voicemail addon configs (if configured)
        self._write_sbc_jsonrpc()
        self._write_voicemail_toml()

        # CC addon generated + addon config lives under proxy.generated_dir
        # (default ./config), i.e. work_dir/config/cc/.
        cc_dir = self.work_dir / "config" / "cc"
        cc_dir.mkdir(parents=True, exist_ok=True)

        agents_rel = self.agents_file or "config/cc/e2e_agents.toml"
        sg_rel = self.skill_groups_file or "config/cc/e2e_skill_groups.toml"
        cc_toml_body = "\n".join([
            '# CC addon config — read by CcAddonState from {generated_dir}/cc/cc.toml',
            f'agents_files = ["{agents_rel}"]',
            f'skillgroup_files = ["{sg_rel}"]',
            'voicemail_extension = "*97"',
            'ivr_extension = "ivr"',
            'callback_retry_secs = 300',
            "",
            "[transfer]",
            'mode = "refer"',
            "",
        ])
        (cc_dir / "cc.toml").write_text(cc_toml_body, encoding="utf-8")

        lines: list[str] = [
            f'log_level = "{self.log_level}"',
            f'database_url = "{self.database_url}"',
            "demo_mode = false",
            f'http_addr = "0.0.0.0:{self.http_port}"',
            "",
            "[proxy]",
            'addr = "0.0.0.0"',
            f"udp_port = {self.sip_port}",
            f"tcp_port = {self.sip_port}",
            'ws_handler = "/ws"',
            'addons = ["' + '", "'.join(self.addons) + '"]',
            "registrar_expires = 3600",
            'codec_strategy = "quality"',
            f"modules = {_toml_value(self.proxy_modules or ['acl', 'auth', 'registrar', 'call'])}",
            "# File sources for routes/trunks/queues/ivr — WITHOUT these the proxy",
            "# reloads 0 entries and no route/trunk/queue/ivr file is ever read.",
            "# Use GLOB patterns: a non-glob path makes the proxy's generated-",
            "# route export overwrite the file itself (data.rs resolve_generated_path).",
            'routes_files = ["config/routes/e2e_*.toml"]',
            'trunks_files = ["config/trunks/e2e_*.toml"]',
            'queues_files = ["config/queue/e2e_*.toml"]',
            'ivr_files = ["config/ivr/*.toml"]',
            "",
        ]
        if self.realms is not None:
            lines.insert(6, f"realms = {_toml_value(self.realms)}")
        if self.media_proxy:
            lines.insert(6, f'media_proxy = "{self.media_proxy}"')
        if self.parallel_fork is not None:
            lines.insert(
                6,
                f"parallel_fork = {'true' if self.parallel_fork else 'false'}",
            )
        if self.max_ring_time is not None:
            lines.insert(6, f"max_ring_time = {self.max_ring_time}")
        if self.session_timer is not None:
            lines.insert(6, f"session_timer = {'true' if self.session_timer else 'false'}")
        if self.session_timer_always is not None:
            lines.insert(
                6,
                f"session_timer_always = {'true' if self.session_timer_always else 'false'}",
            )
        if self.session_expires is not None:
            lines.insert(6, f"session_expires = {self.session_expires}")
        if self.emergency:
            em = self.emergency
            lines.extend([
                "",
                "[proxy.emergency]",
                "enabled = true",
                f"numbers = {_toml_value(em['numbers'])}",
                f'emergency_trunk = "{em["trunk_uri"]}"',
                "",
            ])

        if self.media_config:
            mc = self.media_config
            lines.extend([
                "",
                "[media]",
                f'comfort_noise = {"true" if mc["comfort_noise"] else "false"}',
                f'comfort_noise_level_db = {mc["level_db"]}',
            ])

        # http_router block
        if self.http_router:
            hr = self.http_router
            lines.extend([
                "[proxy.http_router]",
                f'url = "{hr["url"]}"',
                f'timeout_ms = {hr["timeout_ms"]}',
                f'fallback_to_static = {"true" if hr["fallback_to_static"] else "false"}',
            ])
            if hr["headers"]:
                lines.append("[proxy.http_router.headers]")
                for k, v in hr["headers"].items():
                    lines.append(f'"{k}" = "{v}"')
            lines.append("")

        if self.ivr_fallback:
            fb = self.ivr_fallback
            lines.extend(["", "[proxy.ivr_fallback]"])
            if fb.get("default"):
                lines.append(f'default = "{fb["default"]}"')
            for rule in fb.get("rules") or []:
                lines.append("")
                lines.append("[[proxy.ivr_fallback.rules]]")
                if rule.get("name"):
                    lines.append(f'name = "{rule["name"]}"')
                lines.append(f'priority = {int(rule.get("priority", 0))}')
                lines.append(f'target = "{rule["target"]}"')
                match = rule.get("match") or {}
                if match:
                    parts = ", ".join(f'"{k}" = "{v}"' for k, v in match.items())
                    lines.append(f"match = {{ {parts} }}")
            lines.append("")

        # user backend
        self_fwd = self._user_forwarding_lines
        lines.extend([
            "[[proxy.user_backends]]",
            'type = "memory"',
            "",
            "[[proxy.user_backends.users]]",
            "id = 1",
            "enabled = true",
            'username = "1001"',
            'password = "123456"',
            "allow_guest_calls = false",
            "voicemail_disabled = false",
            # is_support_webrtc MUST be false for sipbot-based e2e tests:
            # sipbot sends plain RTP; if true, pbx creates a WebRTC bridge
            # expecting DTLS/SRTP → 0 decryptable packets → no media flows.
            "is_support_webrtc = false",
            *self_fwd("1001"),
            "",
            "[[proxy.user_backends.users]]",
            "id = 2",
            "enabled = true",
            'username = "1002"',
            'password = "123456"',
            "allow_guest_calls = false",
            "voicemail_disabled = false",
            # is_support_webrtc MUST be false for sipbot-based e2e tests:
            # sipbot sends plain RTP; if true, pbx creates a WebRTC bridge
            # expecting DTLS/SRTP → 0 decryptable packets → no media flows.
            "is_support_webrtc = false",
            *self_fwd("1002"),
            "",
            "[[proxy.user_backends.users]]",
            "id = 3",
            "enabled = true",
            'username = "1003"',
            'password = "123456"',
            "allow_guest_calls = false",
            "voicemail_disabled = false",
            # is_support_webrtc MUST be false for sipbot-based e2e tests:
            # sipbot sends plain RTP; if true, pbx creates a WebRTC bridge
            # expecting DTLS/SRTP → 0 decryptable packets → no media flows.
            "is_support_webrtc = false",
            *self_fwd("1003"),
            "",
            # 1004: dedicated SIP account for browser-widget tests. It is NOT
            # listed in config/cc/agents.toml, so it never joins an ACD skill
            # group and cannot steal queue dispatches (a hidden-iframe widget
            # never answers SIP INVITEs → dispatch rings out into fallback).
            "[[proxy.user_backends.users]]",
            "id = 4",
            "enabled = true",
            'username = "1004"',
            'password = "123456"',
            "allow_guest_calls = false",
            "voicemail_disabled = false",
            "is_support_webrtc = false",
            *self_fwd("1004"),
            "",
        ])
        # Additional memory users for the checklist suites (2xxx agents).
        next_id = 5
        for username in self.extra_memory_users:
            lines.extend([
                "[[proxy.user_backends.users]]",
                f"id = {next_id}",
                "enabled = true",
                f'username = "{username}"',
                'password = "123456"',
                "allow_guest_calls = false",
                "voicemail_disabled = false",
                "is_support_webrtc = false",
                *self_fwd(username),
                "",
            ])
            next_id += 1
        for username in self.extra_guest_users:
            lines.extend([
                "[[proxy.user_backends.users]]",
                f"id = {next_id}",
                "enabled = true",
                f'username = "{username}"',
                'password = "123456"',
                "allow_guest_calls = true",
                "voicemail_disabled = false",
                "is_support_webrtc = false",
                *self_fwd(username),
                "",
            ])
            next_id += 1
        lines.extend([
            "[console]",
            'base_path = "/console"',
            'session_secret = "e2e-regression-secret-change-me-32ch!"',
            "allow_registration = false",
            "secure_cookie = false",
            "",
            "# Trusted API token — accepted by phone_auth_middleware as agent 'proxy'",
            "# for /api/cc/* phone-control endpoints, so they don't 401 in e2e.",
            "[[console.api_tokens]]",
            f'token = "{self.rwi_token}"',
            'scopes = ["call","session","media","record","conference","queue","agent","cc"]',
            'description = "e2e regression"',
            "",
            "[recording]",
            "enabled = true",
            f'auto_start = {"true" if self.recording_auto_start else "false"}',
            f'force_file = {"true" if self.recording_force_file else "false"}',
            "",
            "[callrecord]",
            'type = "local"',
            'root = "./config/cdr"',
            "",
            "[sipflow]",
            'type = "local"',
            f'root = "{self.sipflow_root}"',
            'subdirs = "daily"',
            f'engine = "{self.sipflow_engine}"',
            "",
            "[[rwi.tokens]]",
            f'token = "{self.rwi_token}"',
            'scopes = ["call", "session", "media", "record", "conference", "queue"]',
            "",
        ])

        # licenses
        if self.licenses:
            lines.extend([
                "[licenses]",
                "[licenses.addons]",
            ])
            for addon, key in self.licenses.items():
                lines.append(f'{addon} = "{key}"')
            lines.append("")

        # rwi_webhook section
        if self.webhook_url:
            lines.extend([
                "[rwi_webhook]",
                f'url = "{self.webhook_url}"',
                "events = []",
                "",
            ])

        # cc addon
        agents_rel = self.agents_file or "config/cc/e2e_agents.toml"
        sg_rel = self.skill_groups_file or "config/cc/e2e_skill_groups.toml"
        lines.extend([
            "[cc]",
            f'agents_files = ["{agents_rel}"]',
            f'skillgroup_files = ["{sg_rel}"]',
            "",
        ])

        if self.acd_file:
            # Note: acd_policies_files is not a native rustpbx config key;
            # ACD policies are configured via REST API at runtime.
            # We skip writing it to the TOML to avoid parse errors.
            pass

        lines.extend([
            "[cc.cti_webhook]",
            f'events_url = "{self.webhook_url}"',
            'api_key = "e2e-pbx-api-key"',
            "timeout_ms = 5000",
            "",
            "[[ice_servers]]",
            'urls = ["stun:stun.l.google.com:19302"]',
            "",
        ])

        # AMI + outbound sections
        if self.outbound_enabled:
            lines.extend([
                "[ami]",
                'allows = ["127.0.0.1", "::1", "localhost"]',
                "",
                "[outbound]",
                "enabled = true",
                "max_concurrent = 50",
                "default_ring_timeout = 15",
                "default_answer_timeout = 20",
                "default_webhook_timeout = 5",
                "",
            ])

        config_path = self.work_dir / self.config_name
        config_path.write_text("\n".join(lines), encoding="utf-8")
        return config_path


def default_agents_toml() -> str:
    return """\
[[agents]]
agent_id = "1001"
display_name = "Agent 1001 (Regression)"
primary_endpoint = "1001"
skills = ["support", "sales"]
max_concurrency = 3
role = "agent"

[[agents]]
agent_id = "1002"
display_name = "Agent 1002 (Regression)"
primary_endpoint = "1002"
skills = ["support"]
max_concurrency = 3
role = "agent"

[[agents]]
agent_id = "1003"
display_name = "Agent 1003 (Regression)"
primary_endpoint = "1003"
skills = ["support", "sales", "vip"]
max_concurrency = 3
role = "agent"
"""


def default_skill_groups_toml() -> str:
    return """\
[[skill_groups]]
skill_group_id = "support"
skills_required = ["support"]
overflow_groups = []
sla_target_secs = 30
max_wait_secs = 90

[[skill_groups]]
skill_group_id = "sales"
skills_required = ["sales"]
overflow_groups = []
sla_target_secs = 30
max_wait_secs = 90

[[skill_groups]]
skill_group_id = "vip"
skills_required = ["vip"]
overflow_groups = ["support"]
sla_target_secs = 15
max_wait_secs = 60
"""


def default_acd_policies_toml() -> str:
    return """\
[[policies]]
policy_name = "default"
strategy = "longest_idle"
max_queue_size = 100
agent_timeout_secs = 20
wrap_up_time_secs = 10

[[policies]]
policy_name = "round_robin_policy"
strategy = "round_robin"
max_queue_size = 100
agent_timeout_secs = 20
wrap_up_time_secs = 10

[[policies]]
policy_name = "skill_based_policy"
strategy = "skill_based"
max_queue_size = 100
agent_timeout_secs = 20
wrap_up_time_secs = 10

[[policies]]
policy_name = "least_calls_policy"
strategy = "least_calls"
max_queue_size = 100
agent_timeout_secs = 20
wrap_up_time_secs = 10
"""


def ivr_greeting_dtmf_toml(
    route_point: str = "ivr-test",
    greeting_text: str = "Welcome to the test IVR. Press 1 for sales, 2 for support.",
    transfer_target: str = "1001",
    greeting_file: str = "sounds/phone-calling.wav",
) -> str:
    # Use a real audio file for `greeting` (not just TTS greeting_text) so the
    # IVR actually emits RTP — a TTS-only greeting produces no audio when no
    # TTS engine is configured, causing media-flow assertions to fail.
    return f"""\
[ivr]
name = "{route_point}"
ivr_mode = "tree"

[ivr.root]
greeting = "{greeting_file}"
greeting_text = "{greeting_text}"
timeout_ms = 5000
max_retries = 3
timeout_action = {{ type = "repeat" }}
max_retries_action = {{ type = "hangup" }}

[[ivr.root.entries]]
key = "1"
label = "Sales"

[ivr.root.entries.action]
type = "transfer"
target = "{transfer_target}"

[[ivr.root.entries]]
key = "2"
label = "Support"

[ivr.root.entries.action]
type = "queue"
target = "support"
"""


def ivr_collect_webhook_toml(
    route_point: str = "ivr-collect",
    webhook_url: str = "http://127.0.0.1:9999/ivr-webhook",
) -> str:
    return f"""\
[ivr]
name = "{route_point}"
ivr_mode = "tree"

[ivr.root]
greeting = ""
greeting_text = "Please enter your account number followed by hash."
timeout_ms = 10000
max_retries = 3

[[ivr.root.entries]]
key = "*"
label = "Collect"

[ivr.root.entries.action]
type = "collect"
variable = "account_number"
min_digits = 4
max_digits = 12
end_key = "#"
inter_digit_timeout_ms = 5000

[[ivr.root.entries]]
key = "#"
label = "Webhook"

[ivr.root.entries.action]
type = "webhook"
url = "{webhook_url}"
method = "POST"
timeout = 10
"""


def ivr_step_provider_toml(
    route_point: str = "ivr-step",
    provider_url: str = "http://127.0.0.1:9999/ivr-step",
) -> str:
    return f"""\
[ivr]
name = "{route_point}"
ivr_mode = "step"

[ivr.provider]
url = "{provider_url}"
max_retries = 3
retry_delay_ms = 1000
timeout_secs = 10

[ivr.provider.headers]
X-Api-Key = "e2e-test-key"
"""
