"""PRD dual-node cluster harness — remote rustpbx ACD e2e.

Drives the REAL production cluster deployed at RUSTPBX_PRD_HOST
(default 192.168.3.9):

    rustpbx-call  :35060 SIP / :18081 HTTP   (call plane, primary)
    rustpbx-agent :35061 SIP / :18082 HTTP   (agent plane)
    siprouted     :15060 UDP                 (edge LB -> 35060 w5 / 35061 w1)
    rp-mysql      shared DB + DB locator + cc_agent_presence + cc_acd_queue

Agents are driven with the REAL production client (restsend-cli via
``helpers.restsend_agent.RestsendAgent``) registering over SIP to the AGENT
node; callers are sipbot processes entering through a chosen entry point
(call node / agent node / siprouted edge).

Console REST (Bearer ``prd-verify-token``) is used for every assertion and
for config deployment (queues / routes / agents / skill-groups). AMI
endpoints are loopback-only on the Pi, so reloads run over SSH.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid
from typing import Any, Optional

from .restsend_agent import RestsendAgent

logger = logging.getLogger(__name__)

PRD_HOST = os.environ.get("RUSTPBX_PRD_HOST", "192.168.3.9")
PRD_SSH = os.environ.get("RUSTPBX_PRD_SSH", f"pi@{PRD_HOST}")
PRD_TOKEN = os.environ.get("RUSTPBX_PRD_TOKEN", "prd-verify-token")
RESTSEND_CLI = os.environ.get(
    "RESTSEND_CLI", "/Users/pi/workspace/rs/restsend-call/target/debug/restsend-cli"
)

# Per-agent ring timeout (queue strategy.wait_timeout_secs) for the suites.
RING_TIMEOUT_SECS = 6
# How long a dispatched-but-bridged call is allowed to take end-to-end.
DISPATCH_TIMEOUT = 30


class PrdError(AssertionError):
    """Raised for cluster-state assertion failures (keeps pytest output clean)."""


def _skills_list(raw: Any) -> list[str]:
    """Agent skills come back as a list OR as {"list": [...]}."""
    if isinstance(raw, dict):
        raw = raw.get("list") or raw.get("skills") or []
    return list(raw or [])


class PrdNode:
    """REST client for one cluster node's console API."""

    def __init__(self, host: str, http_port: int, sip_port: int, name: str):
        self.host = host
        self.http_port = http_port
        self.sip_port = sip_port
        self.name = name
        self.base = f"http://{host}:{http_port}/rustpbx/api"

    # ── transport ────────────────────────────────────────────────────────
    def request(self, method: str, path: str, body: Any = None,
                timeout: float = 10.0) -> tuple[int, Any]:
        url = f"{self.base}{path}"
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(
            url,
            data=data,
            headers={
                "Authorization": f"Bearer {PRD_TOKEN}",
                "Content-Type": "application/json",
            },
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                raw = resp.read()
                try:
                    return resp.status, json.loads(raw)
                except ValueError:
                    return resp.status, raw.decode(errors="replace")
        except urllib.error.HTTPError as exc:
            raw = exc.read().decode(errors="replace")
            try:
                return exc.code, json.loads(raw)
            except ValueError:
                return exc.code, raw

    def get(self, path: str) -> Any:
        status, body = self.request("GET", path)
        self._check(status, body, path)
        return body

    def post(self, path: str, body: Any = None) -> Any:
        status, resp = self.request("POST", path, body or {})
        self._check(status, resp, path)
        return resp

    def put(self, path: str, body: Any = None) -> Any:
        status, resp = self.request("PUT", path, body or {})
        self._check(status, resp, path)
        return resp

    def delete(self, path: str) -> Any:
        status, resp = self.request("DELETE", path)
        self._check(status, resp, path)
        return resp

    @staticmethod
    def _check(status: int, body: Any, path: str) -> None:
        if status >= 300:
            raise PrdError(f"{path} -> HTTP {status}: {str(body)[:300]}")

    # ── CC state ─────────────────────────────────────────────────────────
    def agents(self) -> list[dict]:
        return self.get("/cc/agents")["data"]

    def agent(self, agent_id: str) -> dict:
        body = self.get(f"/cc/agents/{agent_id}")
        return body.get("data") or body

    def agent_status(self, agent_id: str) -> str:
        try:
            return (self.agent(agent_id).get("status") or "").lower()
        except PrdError:
            return ""

    def agent_current_calls(self, agent_id: str) -> Optional[int]:
        info = self.agent(agent_id)
        cc = info.get("current_calls")
        return int(cc) if cc is not None else None

    def agent_skills(self, agent_id: str) -> list[str]:
        return _skills_list(self.agent(agent_id).get("skills"))

    def set_agent_status(self, agent_id: str, status: str) -> Any:
        return self.post(f"/cc/agents/{agent_id}/status", {"status": status})

    def end_wrapup(self, agent_id: str) -> Any:
        return self.post(f"/cc/agents/{agent_id}/wrapup/end", {})

    def update_agent(self, agent_id: str, payload: dict) -> Any:
        return self.put(f"/cc/agents/{agent_id}", payload)

    def queue_detail(self) -> dict:
        return self.get("/cc/queue-detail")["data"]

    def queued_calls(self) -> list[dict]:
        return self.queue_detail().get("queued_calls") or []

    def ringing_agents(self) -> list[dict]:
        return self.queue_detail().get("ringing_agents") or []

    def active_calls(self) -> list[dict]:
        return self.get("/cc/calls/active")

    def end_call(self, call_id: str) -> Any:
        return self.post(f"/cc/calls/{call_id}/end")

    def active_call_ids(self, callee_contains: Optional[str] = None) -> list[str]:
        calls = self.active_calls()
        if not isinstance(calls, list):
            return []
        ids = []
        for call in calls:
            if not isinstance(call, dict):
                continue
            if callee_contains and callee_contains not in str(call.get("callee", "")):
                continue
            if call.get("call_id"):
                ids.append(call["call_id"])
        return ids

    def only_active_call_id(self, callee_contains: Optional[str] = None) -> str:
        """The single active call (fails loudly on ambiguity)."""
        ids = self.active_call_ids(callee_contains)
        assert len(ids) == 1, f"expected exactly 1 active call, got {ids}"
        return ids[0]

    def end_call_matching(self, callee_contains: str) -> int:
        """End ONLY the calls whose callee matches — queued callers on other
        routes must survive (unlike end_all_active_calls)."""
        n = 0
        for call_id in self.active_call_ids(callee_contains):
            try:
                self.end_call(call_id)
                n += 1
            except PrdError:
                pass
        return n

    def end_all_active_calls(self) -> int:
        n = 0
        try:
            calls = self.active_calls()
        except PrdError:
            return 0
        if not isinstance(calls, list):
            return 0
        for call in calls:
            call_id = call.get("call_id") if isinstance(call, dict) else None
            if call_id:
                try:
                    self.end_call(call_id)
                    n += 1
                except PrdError:
                    pass
        return n

    def realtime(self) -> Any:
        return self.get("/cc/realtime")

    # ── config CRUD ──────────────────────────────────────────────────────
    def create_queue(self, name: str, spec: dict) -> int:
        status, body = self.request("PUT", "/queues", {"name": name, "spec": spec})
        if status in (200, 201):
            return int(body["id"])
        if status == 400 and "already exists" in str(body).lower():
            return self.queue_id_by_name(name)
        raise PrdError(f"create queue {name}: {status} {str(body)[:200]}")

    def queue_id_by_name(self, name: str) -> int:
        body = self.post("/queues", {"page": 1, "per_page": 100})
        for item in body.get("items", []):
            if item.get("name") == name:
                return int(item["id"])
        raise PrdError(f"queue {name} not found")

    def delete_queue(self, queue_id: int) -> None:
        try:
            self.delete(f"/queues/{queue_id}")
        except PrdError as exc:
            logger.warning("delete queue %s: %s", queue_id, exc)

    def create_queue_route(self, name: str, route_point: str, queue: str,
                           priority: int = 50) -> int:
        payload = {
            "name": name,
            "priority": priority,
            "match": {"to.user": f"^{route_point}$"},
            "action": {"select": "rr", "target_type": "queue", "queue_file": queue},
        }
        status, body = self.request("PUT", "/routing", payload)
        if status in (200, 201):
            return int(body["id"])
        if status == 400 and "already exists" in str(body).lower():
            return self.route_id_by_name(name)
        raise PrdError(f"create route {name}: {status} {str(body)[:200]}")

    def route_id_by_name(self, name: str) -> int:
        body = self.post("/routing", {"page": 1, "per_page": 200})
        for item in body.get("items", []):
            if item.get("name") == name:
                return int(item["id"])
        raise PrdError(f"route {name} not found")

    def delete_route(self, route_id: int) -> None:
        try:
            self.delete(f"/routing/{route_id}")
        except PrdError as exc:
            logger.warning("delete route %s: %s", route_id, exc)

    def create_skill_group(self, payload: dict) -> None:
        status, body = self.request("POST", "/cc/skill-groups", payload)
        if status in (200, 201, 409):
            return
        if status == 400 and "already exist" in str(body).lower():
            return
        raise PrdError(f"create skill group: {status} {str(body)[:200]}")

    def delete_skill_group(self, sg_id: str) -> None:
        try:
            self.delete(f"/cc/skill-groups/{sg_id}")
        except PrdError as exc:
            logger.warning("delete skill group %s: %s", sg_id, exc)

    def reload_cc(self) -> None:
        """Reload agents + skill groups + acd on this node (REST, no SSH)."""
        for path in ("/cc/agents/reload", "/cc/skill-groups/reload", "/cc/acd/reload"):
            try:
                self.post(path)
            except PrdError as exc:  # skill-group file reload may 400 — fine
                logger.debug("%s %s: %s", self.name, path, exc)


class PrdCluster:
    """The two-node cluster + edge LB + shared MySQL."""

    def __init__(self) -> None:
        self.host = PRD_HOST
        self.call_node = PrdNode(PRD_HOST, 18081, 35060, "call-node")
        self.agent_node = PrdNode(PRD_HOST, 18082, 35061, "agent-node")
        self.nodes = (self.call_node, self.agent_node)
        self.edge_sip = f"{PRD_HOST}:15060"
        self.call_sip = f"{PRD_HOST}:35060"
        self.agent_sip = f"{PRD_HOST}:35061"
        self._caller_port = 26100

    # ── SSH helpers (AMI is loopback-only on the Pi) ─────────────────────
    def ssh(self, remote_cmd: str, timeout: float = 30.0) -> str:
        proc = subprocess.run(
            ["ssh", PRD_SSH, remote_cmd],
            capture_output=True, text=True, timeout=timeout, check=False,
        )
        if proc.returncode != 0:
            raise PrdError(
                f"ssh failed ({proc.returncode}): {proc.stderr[-300:]}\ncmd: {remote_cmd}"
            )
        return proc.stdout

    def ami(self, path: str, method: str = "POST", body: str = "") -> dict[str, Any]:
        """Invoke an AMI endpoint on BOTH nodes (loopback curl via SSH)."""
        results = {}
        for node in self.nodes:
            out = self.ssh(
                f"curl -s -m 20 -X {method} "
                f"-H 'Content-Type: application/json' "
                f"-d '{body}' "
                f"http://127.0.0.1:{node.http_port}/rustpbx/ami/v1{path}"
            )
            try:
                results[node.name] = json.loads(out)
            except ValueError:
                results[node.name] = out
        return results

    def reload_config(self) -> None:
        """Apply pending console config (routes/queues export) on both nodes."""
        self.ami("/reload/routes")
        self.ami("/reload/queues")
        for node in self.nodes:
            node.reload_cc()

    # ── shared-DB probes (docker exec on the Pi) ─────────────────────────
    def sql(self, query: str) -> list[list[str]]:
        out = self.ssh(
            "docker exec rp-mysql mysql -uroot -p123456 rustpbx_ci "
            f"--batch -e \"{query}\" 2>/dev/null"
        )
        rows = []
        for line in out.splitlines():
            if line.strip():
                rows.append(line.split("\t"))
        return rows

    def acd_queue_rows(self) -> list[dict[str, str]]:
        rows = self.sql(
            "SELECT call_id, queue_id, enqueued_by, claimed_by, claimed_at, "
            "enqueued_at FROM cc_acd_queue"
        )
        if not rows:
            return []
        header = rows[0]
        return [dict(zip(header, row)) for row in rows[1:]]

    def cc_calls(self, limit: int = 10) -> list[dict[str, str]]:
        rows = self.sql(
            "SELECT call_id, queue_id, agent_id, status, wait_time_secs, "
            "talk_time_secs, disposition FROM cc_calls ORDER BY id DESC "
            f"LIMIT {limit}"
        )
        if not rows:
            return []
        header = rows[0]
        return [dict(zip(header, row)) for row in rows[1:]]

    # ── agent state helpers ──────────────────────────────────────────────
    def owner_node_for(self, agent_id: str) -> PrdNode:
        """The node that owns the agent's presence (where it registered).

        Cluster presence is ownership-based: a peer node's REST status write
        applies locally and is REJECTED by the owner. Tests must mutate
        status on the owner — heuristic: the node whose view is not offline.
        """
        for node in (self.agent_node, self.call_node):
            if node.agent_status(agent_id) not in ("offline", ""):
                return node
        return self.call_node

    def set_agent_status(self, agent_id: str, status: str) -> None:
        """Status change on BOTH nodes (owner first, then the dispatcher).

        Cluster presence is ownership-based: the REST write applies to the
        node that receives it (plus the shared DB row), but a peer whose
        local registry already knows the agent does NOT converge non-offline
        states from the DB. Real deployments drive status via SIP PUBLISH
        from the UA (which broadcasts); REST-driven changes must reach every
        dispatching node, so we write both.
        """
        order = [self.owner_node_for(agent_id)]
        order += [n for n in self.nodes if n is not order[0]]
        last_exc: Exception | None = None
        wrote = 0
        for node in order:
            try:
                node.set_agent_status(agent_id, status)
                wrote += 1
            except PrdError as exc:
                last_exc = exc
        if wrote == 0 and last_exc is not None:
            raise last_exc

    def drain(self, agent_ids: list[str], timeout: float = 90.0) -> None:
        """End calls + wrapup until all listed agents are idle/offline."""
        for node in self.nodes:
            node.end_all_active_calls()
        # clear any UA stuck 'in a call' after a server-side end
        try:
            fut = asyncio.run_coroutine_threadsafe(_reset_live_uas(), _SIDECAR.loop)
            fut.result(timeout=5)
        except Exception:  # noqa: BLE001
            pass
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            statuses = [n.agent_status(a) for n in self.nodes for a in agent_ids]
            if all(s in ("idle", "offline", "") for s in statuses):
                return
            for node in self.nodes:
                node.end_all_active_calls()
            for aid in agent_ids:
                status = self.owner_node_for(aid).agent_status(aid)
                if status == "wrapup":
                    try:
                        self.owner_node_for(aid).end_wrapup(aid)
                    except PrdError:
                        pass
                elif status not in ("idle", "offline", ""):
                    try:
                        self.set_agent_status(aid, "idle")
                    except PrdError:
                        pass
            time.sleep(1.5)
        raise PrdError(
            f"drain timeout: {agent_ids} statuses="
            f"{[n.agent_status(a) for n in self.nodes for a in agent_ids]}"
        )

    def assert_agent_status_both_nodes(self, agent_id: str, expected: str,
                                       timeout: float = 20.0) -> None:
        """Agent must reach `expected` on BOTH nodes (shared presence)."""
        deadline = time.monotonic() + timeout
        last: dict[str, str] = {}
        while time.monotonic() < deadline:
            last = {n.name: n.agent_status(agent_id) for n in self.nodes}
            if all(v == expected for v in last.values()):
                return
            time.sleep(0.5)
        raise PrdError(
            f"agent {agent_id} did not reach '{expected}' on both nodes: {last}"
        )

    def wait_for(self, predicate, timeout: float, step: float = 0.5,
                 what: str = "condition") -> Any:
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            last = predicate()
            if last:
                return last
            time.sleep(step)
        raise PrdError(f"timeout ({timeout}s) waiting for {what}; last={last!r}")

    # ── caller factory (unique local SIP port per caller) ────────────────
    def next_caller_port(self) -> int:
        self._caller_port += 1
        return self._caller_port


# ────────────────────────────────────────────────────────────────────────────
# Topology deployment: exclusive skill groups + queues + routes per suite
# ────────────────────────────────────────────────────────────────────────────


class SuiteTopology:
    """Deploys and tears down an isolated ACD topology on the prd cluster.

    - exclusive skill `ex-<tag>-<n>` per group so suites cannot collide
    - one queue per skill group (target skill-group:<id>, ring timeout)
    - one route point per queue (distinct short codes)
    - file agents' skills are extended with the exclusive skills and
      RESTORED on teardown
    """

    def __init__(self, prd: PrdCluster, *, tag: str,
                 groups: list[dict[str, Any]],
                 agents: list[dict[str, Any]],
                 route_base: str,
                 ring_timeout_secs: int = RING_TIMEOUT_SECS,
                 fallback_failure_code: Optional[int] = 480):
        """
        groups: [{id, skills (exclusive auto-added), overflow_groups,
                  max_wait_secs, sla_target_secs}]
        agents: [{id, groups: [group ids], max_concurrency}]
        """
        self.prd = prd
        self.tag = tag
        self.groups = groups
        self.agents = agents
        self.route_base = route_base
        self.ring_timeout_secs = ring_timeout_secs
        self.fallback_failure_code = fallback_failure_code
        self.exclusive_for_group = {
            g["id"]: f"ex-{tag}-{g['id']}" for g in groups
        }
        self._queue_ids: list[int] = []
        self._route_ids: list[int] = []
        self._original_agents: dict[str, dict] = {}

    # ── deploy ───────────────────────────────────────────────────────────
    def skill_group_payload(self, group: dict) -> dict:
        return {
            "skill_group_id": group["id"],
            "display_name": group.get("display_name", group["id"]),
            "skills_required": [self.exclusive_for_group[group["id"]]],
            "overflow_groups": group.get("overflow_groups", []),
            "sla_target_secs": group.get("sla_target_secs", 30),
            "max_wait_secs": group.get("max_wait_secs", 90),
        }

    def deploy(self) -> "SuiteTopology":
        prd = self.prd
        # snapshot original agent definitions for restore
        for agent in self.agents:
            info = prd.call_node.agent(agent["id"])
            self._original_agents[agent["id"]] = {
                "display_name": info.get("display_name"),
                "skills": _skills_list(info.get("skills")),
                "max_concurrency": info.get("max_concurrency") or 1,
            }

        # 1. skill groups (DB) — cache miss falls back to DB at resolve time
        for group in self.groups:
            prd.call_node.create_skill_group(self.skill_group_payload(group))

        # 2. agents get the exclusive skills of their groups
        for agent in self.agents:
            skills = list(self._original_agents[agent["id"]]["skills"])
            for gid in agent["groups"]:
                ex = self.exclusive_for_group[gid]
                if ex not in skills:
                    skills.append(ex)
            prd.call_node.update_agent(agent["id"], {
                "display_name": self._original_agents[agent["id"]]["display_name"],
                "skills": skills,
                "max_concurrency": agent.get(
                    "max_concurrency",
                    self._original_agents[agent["id"]]["max_concurrency"],
                ),
            })

        # 3. queue per group (sequential; per-agent ring timeout)
        for idx, group in enumerate(self.groups):
            spec = {
                "accept_immediately": False,
                "strategy": {
                    "mode": "sequential",
                    "wait_timeout_secs": self.ring_timeout_secs,
                    "targets": [{"uri": f"skill-group:{group['id']}"}],
                },
            }
            if self.fallback_failure_code:
                spec["fallback"] = {
                    "failure_code": self.fallback_failure_code,
                    "failure_reason": f"e2e fallback {group['id']}",
                }
            self._queue_ids.append(
                prd.call_node.create_queue(group["id"], spec)
            )

        # 4. route point per group: <route_base><idx+1>
        self.route_points = {}
        for idx, group in enumerate(self.groups):
            point = f"{self.route_base}{idx + 1}"
            self.route_points[group["id"]] = point
            self._route_ids.append(
                prd.call_node.create_queue_route(
                    f"acd-e2e-{self.tag}-{group['id']}", point, group["id"]
                )
            )

        prd.reload_config()
        return self

    # ── teardown ─────────────────────────────────────────────────────────
    def teardown(self) -> None:
        prd = self.prd
        for node in prd.nodes:
            node.end_all_active_calls()
        for route_id in self._route_ids:
            prd.call_node.delete_route(route_id)
        for queue_id in self._queue_ids:
            prd.call_node.delete_queue(queue_id)
        for group in self.groups:
            prd.call_node.delete_skill_group(group["id"])
        for agent_id, original in self._original_agents.items():
            try:
                prd.call_node.update_agent(agent_id, original)
            except PrdError as exc:
                logger.warning("restore agent %s: %s", agent_id, exc)
        prd.reload_config()

    # ── accessors ────────────────────────────────────────────────────────
    def route_for(self, group_id: str) -> str:
        return self.route_points[group_id]

    def target_uri(self, group_id: str, entry: Optional[str] = None) -> str:
        """SIP target for callers. entry: 'call' | 'agent' | 'edge'."""
        entry = entry or "call"
        target = {
            "call": self.prd.call_sip,
            "agent": self.prd.agent_sip,
            "edge": self.prd.edge_sip,
        }[entry]
        return f"sip:{self.route_for(group_id)}@{target}"


# ────────────────────────────────────────────────────────────────────────────
# restsend agent fleet (production client) registered on the AGENT node
# ────────────────────────────────────────────────────────────────────────────


class _SidecarLoop:
    """One long-lived daemon thread + event loop owning every restsend UA.

    pytest-asyncio gives session-scoped async fixtures and function tests
    DIFFERENT event loops — a subprocess spawned on the fixture's loop gets
    a silently-broken stdin pipe once a test touches it from its own loop.
    Parking the UA processes (and all their coroutines) on this single
    sidecar loop makes every test interact thread-safely via
    `asyncio.wrap_future`, regardless of which pytest loop is current.
    """

    _instance: Optional["_SidecarLoop"] = None

    def __init__(self) -> None:
        self.loop = asyncio.new_event_loop()
        self.thread = threading.Thread(
            target=self._run, name="restsend-sidecar", daemon=True
        )
        self.thread.start()

    def _run(self) -> None:
        asyncio.set_event_loop(self.loop)
        self.loop.run_forever()

    @classmethod
    def get(cls) -> "_SidecarLoop":
        if cls._instance is None:
            cls._instance = cls()
        return cls._instance

    def run(self, coro):
        """Schedule on the sidecar loop; returns a concurrent Future that
        works with `await asyncio.wrap_future(...)` from any loop."""
        return asyncio.run_coroutine_threadsafe(coro, self.loop)


_SIDECAR = _SidecarLoop.get()

# Live UAs on the sidecar loop (for cluster-wide drain resets).
_LIVE_UAS: list["PrdRestsendAgent"] = []


async def _reset_live_uas() -> None:
    """Best-effort `hangup` on every live UA.

    After a server-side end_call the CLI occasionally misses the BYE and
    stays 'in a call' — every later sip_answer is then 'ignored: already
    in a call'. A client-side hangup clears the stuck state.
    """
    for ua in list(_LIVE_UAS):
        try:
            await ua.agent.cmd({"cmd": "hangup"})
        except Exception:  # noqa: BLE001
            pass
    await asyncio.sleep(0.3)


class PrdRestsendAgent:
    """RestsendAgent bound to the prd agent node, with invite accounting.

    All UA work runs on the sidecar loop; every method is awaitable from
    any pytest loop. Event matching is CURSOR-based (see wait_ring).
    """

    def __init__(self, user: str, local_port: int):
        pbx_shim = type("PbxShim", (), {"host": PRD_HOST, "sip_port": 35061})()
        self.user = user
        self.port = local_port
        self.agent = RestsendAgent(pbx_shim, user, local_port=local_port)
        self._cursor = 0  # index into agent.events; matched-consumed marker

    # ── sidecar plumbing ─────────────────────────────────────────────────
    async def _start_impl(self) -> None:
        try:
            await self.agent.start()
            if not await self.agent.register(expires=600):
                raise PrdError(
                    f"restsend {self.user} failed to register\n"
                    f"stderr: {self.agent.stderr_text()[-800:]}"
                )
        except PrdError:
            raise
        except Exception as exc:  # noqa: BLE001
            raise PrdError(
                f"restsend {self.user} spawn/register failed: {exc}\n"
                f"stderr: {self.agent.stderr_text()[-800:]}"
            ) from exc
        await self.agent.publish_idle()
        _LIVE_UAS.append(self)

    async def start(self) -> None:
        await asyncio.wrap_future(_SIDECAR.run(self._start_impl()))

    async def stop(self) -> None:
        await asyncio.wrap_future(_SIDECAR.run(self.agent.stop()))
        if self in _LIVE_UAS:
            _LIVE_UAS.remove(self)

    # event accounting (runs on the sidecar loop)
    @property
    def events(self) -> list[dict]:
        return self.agent.events

    def invite_count(self) -> int:
        """Total INVITEs seen so far (all calls, lifetime of this UA)."""
        return sum(1 for e in self.agent.events if e.get("evt") == "sip_incoming")

    async def _wait_ring_impl(self, timeout: float) -> Optional[dict]:
        loop = asyncio.get_event_loop()
        deadline = loop.time() + timeout
        while True:
            for idx, ev in enumerate(self.agent.events[self._cursor:]):
                if ev.get("evt") == "sip_incoming":
                    self._cursor += idx + 1
                    return ev
            if loop.time() >= deadline:
                return None
            await asyncio.sleep(0.2)

    async def wait_ring(self, timeout: float = DISPATCH_TIMEOUT) -> Optional[dict]:
        """Wait for a NEW incoming call (sip_incoming not yet consumed)."""
        return await asyncio.wrap_future(_SIDECAR.run(self._wait_ring_impl(timeout)))

    async def wait_quiet(self, quiet_secs: float) -> bool:
        """True when NO new invite lands within the window."""
        before = self.invite_count()
        await asyncio.sleep(quiet_secs)
        return self.invite_count() == before

    async def _answer_cmd_impl(self) -> bool:
        """Send sip_answer (retried) and wait for a FRESH connected event."""
        base = len(self.agent.events)  # only events after this count
        for _attempt in range(3):
            await self.agent.cmd({"cmd": "sip_answer"})
            deadline = asyncio.get_event_loop().time() + 5
            while asyncio.get_event_loop().time() < deadline:
                if any(e.get("evt") == "state_changed"
                       and e.get("name") == "connected"
                       for e in self.agent.events[base:]):
                    return True
                await asyncio.sleep(0.2)
        return any(e.get("evt") == "state_changed"
                   and e.get("name") == "connected"
                   for e in self.agent.events[base:])

    async def answer_pending(self) -> bool:
        """Answer the call that a previous `wait_ring` already reported.

        Use after wait_ring(); `answer_next` would wait for a SECOND invite
        that never comes (cursor already consumed the ring event).
        """
        await asyncio.sleep(0.3)  # let the CLI settle into `ringing`
        return await asyncio.wrap_future(_SIDECAR.run(self._answer_cmd_impl()))

    async def reset(self) -> None:
        """Clear a possibly stuck 'in a call' state (client-side hangup).

        Safe when no dispatch is currently ringing this UA — call it right
        AFTER a server-side end_call, BEFORE the next expected dispatch.
        """
        def _impl():
            async def __impl():
                try:
                    await self.agent.cmd({"cmd": "hangup"})
                except Exception:  # noqa: BLE001
                    pass
                await asyncio.sleep(0.2)
            return __impl()
        await asyncio.wrap_future(_SIDECAR.run(_impl()))

    async def _answer_impl(self, timeout: float) -> bool:
        ring = await self._wait_ring_impl(timeout)
        if ring is None:
            print(f"\n[restsend {self.user}] answer_next: NO RING within {timeout}s; "
                  f"events={self.agent.events[-6:]}\n"
                  f"stderr_tail={self.agent.stderr_text()[-500:]}")
            return False
        await asyncio.sleep(0.3)  # let the CLI settle into `ringing`
        result = await self._answer_cmd_impl()
        if not result:
            print(f"\n[restsend {self.user}] answer_next: ring={ring} but no connect; "
                  f"events={self.agent.events[-8:]}\n"
                  f"stderr_tail={self.agent.stderr_text()[-600:]}")
        return result

    async def answer_next(self, timeout: float = DISPATCH_TIMEOUT) -> bool:
        """Wait for a NEW incoming call and answer it."""
        return await asyncio.wrap_future(_SIDECAR.run(self._answer_impl(timeout)))

    async def hangup(self) -> None:
        await asyncio.wrap_future(
            _SIDECAR.run(self.agent.cmd({"cmd": "hangup"}))
        )
