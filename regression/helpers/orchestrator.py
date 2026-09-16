"""Scenario orchestrator — executes a :class:`Scenario` against the session pbx.

Each scenario step is dispatched to a small handler that drives the existing
RWI webhook / RWI WebSocket / REST fixtures. Execution is wrapped in
``try/finally`` so that:

  - shared skill-group mutations are restored to their pre-scenario state
    (the session DB persists across tests, and `seed_default_agents` only
    creates — never overwrites — so leaks would pollute later tests), and
  - all spawned sipbot UAs are torn down.
"""

from __future__ import annotations

import asyncio
import logging
import re
import uuid
from typing import Any, Optional

from .agent_simulator import AgentSimulator
from .scenario import Scenario

logger = logging.getLogger(__name__)

BUILTINS = {
    "pbx.sip_addr",
    "pbx.sip_proxy",
    "pbx.host",
    "pbx.sip_port",
    "pbx.http_url",
    "pbx.rwi_token",
}


def _dig(obj: Any, path: str) -> Any:
    cur = obj
    for part in path.split("."):
        if isinstance(cur, dict):
            cur = cur.get(part)
        elif isinstance(cur, list):
            try:
                cur = cur[int(part)]
            except (ValueError, IndexError):
                return None
        elif cur is not None and not isinstance(cur, (str, int, float, bool)):
            cur = getattr(cur, part, None)
        else:
            return None
        if cur is None:
            return None
    return cur


def _coerce(value: Any, expected: Any) -> Any:
    if expected is None:
        return value
    if isinstance(expected, bool):
        return bool(value) if not isinstance(value, bool) else value
    if isinstance(expected, int):
        try:
            return int(value)
        except (TypeError, ValueError):
            return value
    if isinstance(expected, float):
        try:
            return float(value)
        except (TypeError, ValueError):
            return value
    return value


def _preview(value: Any, limit: int = 120) -> Any:
    """Truncate a step result for the run trace (keeps the flow file small)."""
    text = str(value)
    if len(text) > limit:
        return f"{text[:limit]}…"
    return value


class Orchestrator:
    """Runs scenario steps against the session-scoped pbx fixtures."""

    def __init__(self, pbx, sipbot_pool, api, event_checker):
        self.pbx = pbx
        self.pool = sipbot_pool
        self.api = api
        self.event_checker = event_checker
        self.agents = AgentSimulator(pbx, sipbot_pool, api)
        self.vars: dict[str, Any] = {}
        self._restore_sgs: dict[str, dict] = {}
        self._created_sgs: list[str] = []

    # ---- public ----

    async def run(self, scenario: Scenario) -> dict:
        outcome: dict[str, Any] = {
            "scenario": scenario.name,
            "steps": [],
            "ok": True,
            "error": None,
        }
        try:
            await self._setup(scenario.setup)
            for step in scenario.steps:
                entry: dict[str, Any] = {
                    "op": step["op"],
                    "label": step.get("label", step["op"]),
                }
                try:
                    result = await self._exec(step)
                    if result is not None:
                        entry["result"] = _preview(result)
                except Exception as exc:  # noqa: BLE001 — report first failure
                    entry["error"] = f"{type(exc).__name__}: {exc}"
                    outcome["ok"] = False
                    outcome["error"] = (
                        f"scenario '{scenario.name}' failed at step "
                        f"{len(outcome['steps'])} '{step.get('label', step['op'])}': {exc}"
                    )
                    outcome["steps"].append(entry)
                    break
                outcome["steps"].append(entry)
        finally:
            await self._restore()
        return outcome

    # ---- setup ----

    async def _setup(self, setup: dict) -> None:
        for agent in setup.get("agents", []):
            await self._create_agent_idempotent(agent)
        for sg in setup.get("skill_groups", []):
            await self._ensure_skill_group(sg)

    async def _create_agent_idempotent(self, agent: dict) -> None:
        try:
            await self.api.create_agent(agent)
        except Exception as exc:  # noqa: BLE001 — 409/400 duplicate is fine
            msg = str(exc)
            if not any(tok in msg for tok in ("409", "400", "already")):
                raise

    async def _ensure_skill_group(self, sg: dict) -> None:
        sgid = sg["skill_group_id"]
        existing = await self.api.get(f"/api/cc/skill-groups/{sgid}")
        if isinstance(existing, dict) and "data" in existing:
            self._snapshot_skill_group(sgid, existing["data"])
            body = dict(sg)
            body.pop("skill_group_id", None)
            await self._update_skill_group(sgid, body)
        else:
            body = dict(sg)
            body.pop("skill_group_id", None)
            try:
                await self.api.create_skill_group(body)
                self._created_sgs.append(sgid)
            except Exception as exc:  # noqa: BLE001
                msg = str(exc)
                if not any(tok in msg for tok in ("409", "400", "already")):
                    raise

    async def _update_skill_group(self, sgid: str, body: dict) -> None:
        self._snapshot_skill_group(sgid, await self._fetch_skill_group(sgid))
        await self.api.put(f"/api/cc/skill-groups/{sgid}", body)

    async def _fetch_skill_group(self, sgid: str) -> Optional[dict]:
        try:
            resp = await self.api.get(f"/api/cc/skill-groups/{sgid}")
        except Exception:  # noqa: BLE001
            return None
        if isinstance(resp, dict):
            return resp.get("data") or resp
        return None

    def _snapshot_skill_group(self, sgid: str, data: Optional[dict]) -> None:
        if sgid in self._restore_sgs or not data:
            return
        self._restore_sgs[sgid] = {
            "skills_required": data.get("skills_required") or [],
            "overflow_groups": data.get("overflow_groups") or [],
            "sla_target_secs": data.get("sla_target_secs"),
            "max_wait_secs": data.get("max_wait_secs"),
            "metadata": data.get("metadata"),
            "acd_policy": data.get("acd_policy"),
        }

    # ---- step dispatch ----

    async def _exec(self, step: dict) -> Any:
        op = step["op"]
        handler = getattr(self, f"_op_{op}", None)
        if handler is None:
            raise ValueError(f"no handler for op '{op}'")
        return await handler(step)

    # setup ops

    async def _op_seed_agents(self, step: dict) -> None:
        for agent in step["agents"]:
            await self._create_agent_idempotent(agent)

    async def _op_update_skill_group(self, step: dict) -> None:
        sgid = step["skill_group_id"]
        await self._update_skill_group(sgid, dict(step["body"]))

    async def _op_create_skill_group(self, step: dict) -> None:
        body = dict(step["body"])
        try:
            await self.api.create_skill_group(body)
            self._created_sgs.append(step["skill_group_id"])
        except Exception as exc:  # noqa: BLE001 — duplicate is fine
            msg = str(exc)
            if not any(tok in msg for tok in ("409", "400", "already")):
                raise

    # agent lifecycle ops

    async def _op_agent_online(self, step: dict) -> str:
        bot = await self.agents.online(
            step["username"],
            answer_mode=step.get("answer_mode", "echo"),
            hangup_after=step.get("hangup_after"),
            dtmf_flows=step.get("dtmf_flows"),
            ring_secs=step.get("ring_secs", 1),
            set_idle=bool(step.get("set_idle", False)),
            port=step.get("port"),
        )
        if step.get("store"):
            self.vars[step["store"]] = bot
        return f"{step['username']} online"

    async def _op_agent_offline(self, step: dict) -> str:
        await self.agents.offline(step["username"])
        return f"{step['username']} offline"

    async def _op_agent_status(self, step: dict) -> None:
        await self.agents.status(step["username"], step["status"])

    # call control ops

    async def _op_originate(self, step: dict) -> None:
        destination = self._resolve(step["destination"])
        caller_id = step.get("caller_id")
        if step.get("via") == "sipbot":
            call_id = await self._originate_via_sipbot(step, destination, caller_id)
        else:
            call_id = self._resolve(step["call_id"])
            await self.event_checker.rwi.originate(
                call_id=call_id,
                destination=destination,
                caller_id=caller_id,
                timeout_secs=step.get("timeout_secs", 30),
            )
        if step.get("store"):
            self.vars[step["store"]] = call_id

    async def _originate_via_sipbot(
        self, step: dict, destination: str, caller_id: Optional[str]
    ) -> str:
        """Place an authenticated inbound call with a sipbot UA.

        Plain RWI originate dials the callee URI directly; a self-INVITE to the
        proxy (route/app targets like ``sip:8888@`` / ``sip:ivr-test@``) is
        407'd because the RWI UAC is not an authenticated endpoint. A sipbot
        caller registers nothing but answers the proxy's Digest challenge with
        a known user's credentials, so route/app dispatch works exactly like a
        real phone. The server assigns the call_id, so we bind it into the
        scenario vars from the matching ``call_created`` event.
        """
        caller_id = caller_id or "1001"
        self.pool.caller(
            target=destination,
            username=caller_id,
            password="123456",
            hangup=step.get("hangup_after", 45),
        )
        var_name: Optional[str] = None
        m = re.fullmatch(r"\{([a-zA-Z0-9_.-]+)\}", step["call_id"])
        if m:
            var_name = m.group(1)
        already_bound = set(self.vars.values())
        loop = asyncio.get_event_loop()
        deadline = loop.time() + step.get("timeout_secs", 30)
        while loop.time() < deadline:
            for ev in self.event_checker.webhook.all_events():
                if ev.event_type != "call_created":
                    continue
                if ev.call_id in already_bound:
                    continue
                payload = ev.payload if isinstance(ev.payload, dict) else {}
                if payload.get("callee") != destination:
                    continue
                caller = payload.get("caller") or ""
                if caller_id not in caller:
                    continue
                if var_name:
                    self.vars[var_name] = ev.call_id
                return ev.call_id
            await asyncio.sleep(0.15)
        raise AssertionError(
            f"sipbot caller to {destination} never produced a matching "
            f"call_created within {step.get('timeout_secs', 30)}s"
        )

    async def _op_dtmf(self, step: dict) -> None:
        await self.event_checker.rwi.send_dtmf(
            self._resolve(step["call_id"]), step["digits"]
        )

    async def _op_hangup(self, step: dict) -> None:
        await self.event_checker.rwi.hangup(self._resolve(step["call_id"]))

    async def _op_end_call(self, step: dict) -> None:
        await self.api.end_call(self._resolve(step["call_id"]))

    async def _op_blind_transfer(self, step: dict) -> None:
        await self.api.blind_transfer(
            self._resolve(step["call_id"]), self._resolve(step["target"])
        )

    async def _op_consult(self, step: dict) -> None:
        status, body = await self.api.raw_request(
            "POST",
            f"/api/cc/calls/{self._resolve(step['call_id'])}/consult",
            {"target": self._resolve(step["target"])},
        )
        if status != 200:
            raise AssertionError(f"/consult returned HTTP {status}: {body!r:.200}")
        tid = body.get("transfer_id") if isinstance(body, dict) else None
        if not tid:
            raise AssertionError(f"/consult missing transfer_id: {body!r:.200}")
        if step.get("store"):
            self.vars[step["store"]] = tid
        return tid

    async def _op_consult_connected(self, step: dict) -> None:
        status, body = await self.api.raw_request(
            "PUT",
            f"/api/cc/calls/{self._resolve(step['call_id'])}/consult/"
            f"{self._resolve(step['tid'])}/connected",
            {"session_b": self._resolve(step["session_b"])},
        )
        if status != 200:
            raise AssertionError(f"/consult/connected returned HTTP {status}: {body!r:.200}")

    async def _op_consult_merge(self, step: dict) -> Any:
        status, body = await self.api.raw_request(
            "POST",
            f"/api/cc/calls/{self._resolve(step['call_id'])}/consult/"
            f"{self._resolve(step['tid'])}/merge",
            {},
        )
        if status != 200:
            raise AssertionError(f"/consult/merge returned HTTP {status}: {body!r:.200}")
        conf_id = body.get("conf_id") if isinstance(body, dict) else None
        if step.get("store"):
            self.vars[step["store"]] = conf_id
        return conf_id

    async def _op_consult_complete(self, step: dict) -> None:
        status, body = await self.api.raw_request(
            "POST",
            f"/api/cc/calls/{self._resolve(step['call_id'])}/consult/"
            f"{self._resolve(step['tid'])}/complete",
            {},
        )
        if status != 200:
            raise AssertionError(f"/consult/complete returned HTTP {status}: {body!r:.200}")

    # conference ops

    async def _op_conference_create(self, step: dict) -> str:
        conf_id = self._resolve(step["conf_id"])
        await self.event_checker.rwi.conference_create(conf_id)
        if step.get("store"):
            self.vars[step["store"]] = conf_id
        return conf_id

    async def _op_conference_add(self, step: dict) -> None:
        await self.event_checker.rwi.conference_add(
            self._resolve(step["conf_id"]), self._resolve(step["call_id"])
        )

    async def _op_conference_mute(self, step: dict) -> None:
        await self.event_checker.rwi.conference_mute(
            self._resolve(step["conf_id"]), self._resolve(step["call_id"])
        )

    async def _op_conference_unmute(self, step: dict) -> None:
        await self.event_checker.rwi.conference_unmute(
            self._resolve(step["conf_id"]), self._resolve(step["call_id"])
        )

    async def _op_conference_remove(self, step: dict) -> None:
        await self.event_checker.rwi.conference_remove(
            self._resolve(step["conf_id"]), self._resolve(step["call_id"])
        )

    async def _op_conference_destroy(self, step: dict) -> None:
        await self.event_checker.rwi.conference_destroy(self._resolve(step["conf_id"]))

    # event assertion ops

    async def _op_wait_webhook(self, step: dict) -> dict:
        call_id = self._resolve(step["call_id"]) if step.get("call_id") else None
        match = step.get("match")
        if match:
            match = {
                self._resolve(path): self._resolve(expected)
                for path, expected in match.items()
            }
        ev = await self.event_checker.webhook.wait_for_event(
            step["event"],
            timeout=step.get("timeout", 20.0),
            call_id=call_id,
            occurrence=step.get("occurrence", 1),
            match=match,
        )
        if ev is None:
            raise AssertionError(
                f"webhook event '{step['event']}' (occurrence "
                f"{step.get('occurrence', 1)}) not seen within "
                f"{step.get('timeout', 20.0)}s. got: "
                f"{self.event_checker.webhook.event_types()}"
            )
        if step.get("store"):
            self.vars[step["store"]] = ev
        return ev.event_type

    async def _op_wait_rwi(self, step: dict) -> dict:
        ev = await self.event_checker.rwi.wait_for_event(
            step["event"], timeout=step.get("timeout", 20.0)
        )
        if ev is None:
            raise AssertionError(
                f"RWI event '{step['event']}' not seen within "
                f"{step.get('timeout', 20.0)}s. got: "
                f"{[e.get('event_type') for e in self.event_checker.rwi.events]}"
            )
        if step.get("store"):
            self.vars[step["store"]] = ev
        return ev.get("event_type")

    async def _op_wait_sequence(self, step: dict) -> None:
        call_id = self._resolve(step["call_id"]) if step.get("call_id") else None
        ok = await self.event_checker.webhook.wait_for_sequence(
            list(step["events"]), timeout=step.get("timeout", 30.0), call_id=call_id
        )
        if not ok:
            raise AssertionError(
                f"webhook sequence {step['events']} not seen within "
                f"{step.get('timeout', 30.0)}s. got: "
                f"{self.event_checker.webhook.event_types()}"
            )

    async def _op_assert_field(self, step: dict) -> None:
        value = _dig(self.vars.get(step["var"]), step["path"])
        if "equals" in step:
            expected = self._resolve(step["equals"])
            actual = _coerce(value, expected)
            if actual != expected:
                raise AssertionError(
                    f"{step['var']}.{step['path']} = {value!r} (want {expected!r})"
                )
        if "not_equal" in step:
            expected = self._resolve(step["not_equal"])
            actual = _coerce(value, expected)
            if actual == expected:
                raise AssertionError(
                    f"{step['var']}.{step['path']} = {value!r} (must not equal {expected!r})"
                )
        if "one_of" in step:
            allowed = [self._resolve(v) for v in step["one_of"]]
            if value not in allowed:
                raise AssertionError(
                    f"{step['var']}.{step['path']} = {value!r} (want one of {allowed})"
                )

    async def _op_assert_no_webhook(self, step: dict) -> None:
        wait = step.get("wait", 3.0)
        ev = await self.event_checker.webhook.wait_for_event(step["event"], timeout=wait)
        if ev is not None:
            raise AssertionError(f"unexpected webhook event '{step['event']}' arrived")

    async def _op_check_cdr(self, step: dict) -> Any:
        call_id = self._resolve(step["call_id"])
        want = step.get("equals")
        timeout = step.get("timeout", 30.0)
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while loop.time() < deadline:
            try:
                detail = await self.api.get(f"/api/cc/calls/{call_id}")
            except Exception:  # noqa: BLE001 — call may not be finalized yet
                detail = None
            cdr = detail.get("data", detail) if isinstance(detail, dict) else None
            value = _dig(cdr, step["path"]) if cdr else None
            if value is not None:
                if want is None:
                    return value
                if _coerce(value, want) == want:
                    return value
            await asyncio.sleep(1)
        raise AssertionError(
            f"CDR field '{step['path']}' for {call_id} never became {want!r} "
            f"within {timeout}s (last cdr: {cdr!r:.200})"
        )

    # misc ops

    async def _op_sleep(self, step: dict) -> None:
        await asyncio.sleep(float(step["secs"]))

    async def _op_log(self, step: dict) -> str:
        return str(step["msg"])

    # ---- templating ----

    def _resolve(self, value: Any) -> Any:
        if not isinstance(value, str):
            return value
        return re.sub(
            r"\{([a-zA-Z0-9_.-]+)\}",
            lambda m: str(self._lookup(m.group(1))),
            value,
        )

    def _lookup(self, name: str) -> Any:
        if name == "uid":
            return f"scn-{uuid.uuid4().hex[:8]}"
        if name in BUILTINS:
            return {
                "pbx.sip_addr": self.pbx.sip_addr,
                "pbx.sip_proxy": f"{self.pbx.host}:{self.pbx.sip_port}",
                "pbx.host": self.pbx.host,
                "pbx.sip_port": self.pbx.sip_port,
                "pbx.http_url": self.pbx.http_url,
                "pbx.rwi_token": self.pbx.rwi_token,
            }[name]
        if name in self.vars:
            value = self.vars[name]
            return value.call_id if hasattr(value, "call_id") else value
        raise KeyError(f"unknown template variable '{name}'")

    # ---- teardown ----

    async def _restore(self) -> None:
        """Restore mutated skill-groups, drop created ones, take agents offline."""
        for sgid, original in self._restore_sgs.items():
            try:
                body = {k: v for k, v in original.items() if v is not None}
                await self.api.put(f"/api/cc/skill-groups/{sgid}", body)
                logger.info("restored skill-group %s", sgid)
            except Exception as exc:  # noqa: BLE001
                logger.warning("failed to restore skill-group %s: %s", sgid, exc)
        for sgid in self._created_sgs:
            try:
                await self.api.delete(f"/api/cc/skill-groups/{sgid}")
                logger.info("removed created skill-group %s", sgid)
            except Exception as exc:  # noqa: BLE001
                logger.warning("failed to delete skill-group %s: %s", sgid, exc)
        # End every active call first (the PBX needs an explicit hangup; a
        # killed sipbot just stops answering UDP, so the call would linger and
        # keep the agent Busy). Ending the calls lets the agent session settle
        # out of Busy/Ringing before we deregister the UAs.
        try:
            active = await self.api.list_active_calls()
            for call in active or []:
                cid = call.get("call_id") if isinstance(call, dict) else call
                if not cid:
                    continue
                try:
                    await self.api.end_call(cid)
                    logger.info("ended active call %s", cid)
                except Exception as exc:  # noqa: BLE001
                    logger.warning("end_call %s failed: %s", cid, exc)
        except Exception as exc:  # noqa: BLE001
            logger.warning("list_active_calls failed: %s", exc)
        await asyncio.sleep(2.0)
        # Terminate every UA so no registration lingers, then take agents to a
        # confirmed Offline so stale Idle candidates never leak into the next
        # test's ACD pool.
        usernames = list(self.agents.registered())
        self.agents.terminate_all()
        await self.agents.offline_all(usernames)
