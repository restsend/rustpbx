"""VQ (virtual-queue) agent lifecycle simulator.

Wraps ``sipbot`` UAs so scenario orchestrators can bring CC agents online /
offline and change presence exactly like a real agent phone:

  - ``online()`` : register the sipbot endpoint. The registrar bridge marks the
    CC agent ``Idle`` on registration (src/addons/cc/registrar_bridge.rs), so
    the ACD can dispatch to it.
  - ``offline()``: terminate the sipbot → deregistration flips the agent
    offline; we additionally POST an explicit ``offline`` status so the state
    is correct even when the registrar event is delayed.
  - ``status()`` : explicit REST status override (offline / idle / dnd / ...).
"""

from __future__ import annotations

import asyncio
import logging
from typing import Optional

from .sipbot import SipBotProcess, SipBotPool

logger = logging.getLogger(__name__)


class AgentSimulator:
    """Brings CC agents online/offline against the session pbx."""

    def __init__(
        self,
        pbx,
        sipbot_pool: SipBotPool,
        api,
        *,
        base_port: int = 17100,
    ):
        self.pbx = pbx
        self.pool = sipbot_pool
        self.api = api
        self.base_port = base_port
        self._agents: dict[str, SipBotProcess] = {}

    async def online(
        self,
        username: str,
        *,
        answer_mode: str = "echo",
        hangup_after: Optional[int] = None,
        dtmf_flows: Optional[str] = None,
        ring_secs: int = 1,
        set_idle: bool = False,
        port: Optional[int] = None,
    ) -> SipBotProcess:
        """Register a sipbot as ``username`` and wait for the agent to go Idle."""
        # Kill any stale bot from a previous test that still holds this user —
        # otherwise the PBX may fork the INVITE to the dead endpoint.
        self.pool.terminate_user(username)
        port = port or self._default_port(username)
        bot = self.pool.callee(
            host=self.pbx.host,
            port=port,
            username=username,
            password="123456",
            register=True,
            proxy=f"{self.pbx.host}:{self.pbx.sip_port}",
            domain=self.pbx.host,
            ring_secs=ring_secs,
            answer_mode=answer_mode,
            hangup_after=hangup_after,
            dtmf_flows=dtmf_flows,
        )
        self._agents[username] = bot
        # Wait until the registrar-bridge transition lands so the ACD reliably
        # sees the agent as an Idle candidate before any call is originated.
        # Under full-suite load the first REGISTER can be swallowed (e.g. the
        # agent was mid-wrapup when the old bot died, or the bridge raced the
        # status write) — if the deadline passes without Idle, force the agent
        # Offline and re-register once before giving up.
        loop = asyncio.get_running_loop()
        deadline = loop.time() + 10.0
        reregistered = False
        while True:
            status = await self._status(username)
            if status == "idle":
                break
            if loop.time() >= deadline:
                if reregistered:
                    raise RuntimeError(
                        f"agent {username} did not reach Idle after registration "
                        f"(last status {status!r})"
                    )
                # One clean retry: drop the bot, force Offline, register again.
                reregistered = True
                logger.warning(
                    "agent %s stuck in %r — forcing offline and re-registering",
                    username,
                    status,
                )
                self.pool.terminate_user(username)
                try:
                    await self.api.update_agent_status(username, "offline")
                except Exception:
                    pass
                bot = self.pool.callee(
                    host=self.pbx.host,
                    port=port,
                    username=username,
                    password="123456",
                    register=True,
                    proxy=f"{self.pbx.host}:{self.pbx.sip_port}",
                    domain=self.pbx.host,
                    ring_secs=ring_secs,
                    answer_mode=answer_mode,
                    hangup_after=hangup_after,
                    dtmf_flows=dtmf_flows,
                )
                self._agents[username] = bot
                deadline = loop.time() + 10.0
                continue
            await asyncio.sleep(0.5)
        if set_idle:
            try:
                await self.api.update_agent_status(username, "idle")
            except Exception as exc:  # noqa: BLE001 — best-effort
                logger.warning("set idle for agent %s failed: %s", username, exc)
        return bot

    async def offline(self, username: str, *, wait_secs: float = 20.0) -> None:
        """Deregister ``username`` and force its CC status to offline.

        A killed sipbot does NOT unregister (no REGISTER expires=0), so the
        pbx keeps the agent Idle until the registration lease lapses — leaving
        a stale ACD candidate for the next test. We therefore drive the agent
        to a confirmed Offline via the status API, coercing Wrapup with
        ``end_wrapup`` and retrying until the registry reports Offline.
        """
        self.pool.terminate_user(username)
        self._agents.pop(username, None)
        loop = asyncio.get_running_loop()
        deadline = loop.time() + wait_secs
        last_err: Optional[Exception] = None
        while True:
            try:
                await self.api.update_agent_status(username, "offline")
                last_err = None
            except Exception as exc:  # noqa: BLE001 — transition may be invalid while in-call
                last_err = exc
                # Coerce Wrapup -> Idle so Idle -> Offline becomes valid.
                try:
                    await self.api.end_agent_wrapup(username)
                except Exception:  # noqa: BLE001
                    pass
            # The kill above may already have flipped the agent Offline (in
            # which case the POST 400s as offline -> offline) — treat a
            # confirmed Offline as success.
            if await self._status(username) == "offline":
                last_err = None
                break
            if loop.time() >= deadline:
                break
            await asyncio.sleep(1.0)
        if last_err is not None:
            logger.warning(
                "set offline for agent %s failed after retries: %s", username, last_err
            )

    async def _status(self, username: str) -> Optional[str]:
        try:
            agent = await self.api.get_agent(username)
        except Exception:  # noqa: BLE001
            return None
        if isinstance(agent, dict):
            return agent.get("status")
        return None

    async def offline_all(self, usernames: Optional[list[str]] = None) -> None:
        """Take every currently-registered agent offline (teardown helper)."""
        targets = usernames if usernames is not None else list(self._agents)
        for username in targets:
            await self.offline(username)

    async def status(self, username: str, status: str) -> None:
        await self.api.update_agent_status(username, status)

    async def wait_for_status(
        self,
        username: str,
        expected: str,
        *,
        wait_secs: float = 15.0,
        base_status_only: bool = True,
    ) -> str:
        """Poll until the CC status of ``username`` equals ``expected``.

        Returns the last observed status; callers should assert on it (the
        helper never raises on mismatch so tests control failure messaging).
        ``base_status_only`` compares the base name ("wrapup", "away")
        ignoring call-id / reason decorations ("wrapup:call-1", "away:x").
        """
        loop = asyncio.get_running_loop()
        deadline = loop.time() + wait_secs
        observed: Optional[str] = None
        while True:
            observed = await self._status(username)
            actual = observed.split(":")[0] if (observed and base_status_only) else observed
            if actual == expected:
                return actual or ""
            if loop.time() >= deadline:
                return observed or ""
            await asyncio.sleep(0.5)

    async def ensure_state(
        self,
        username: str,
        status: str,
        *,
        wait_secs: float = 15.0,
    ) -> None:
        """Drive ``username`` to ``status`` and confirm — strict setup helper.

        Coerces Wrapup → Idle (end_wrapup) like :meth:`offline` when an
        invalid-transition error blocks the way, then polls until the
        registry actually reports the target state; raises on timeout so a
        misconfigured setup fails the test loudly instead of hanging.
        """
        loop = asyncio.get_running_loop()
        deadline = loop.time() + wait_secs
        last_err: Optional[Exception] = None
        while True:
            try:
                await self.api.update_agent_status(username, status)
                last_err = None
            except Exception as exc:  # noqa: BLE001 — transition may be invalid
                last_err = exc
                try:
                    await self.api.end_agent_wrapup(username)
                except Exception:  # noqa: BLE001
                    pass
            observed = await self._status(username)
            if observed is not None and observed.split(":")[0] == status:
                return
            if loop.time() >= deadline:
                break
            await asyncio.sleep(1.0)
        raise AssertionError(
            f"agent {username} never reached state {status!r} "
            f"(last={observed!r}, err={last_err})"
        )


    def get(self, username: str) -> Optional[SipBotProcess]:
        return self._agents.get(username)

    def registered(self) -> list[str]:
        return list(self._agents)

    def terminate_all(self) -> None:
        self.pool.terminate_all()
        self._agents.clear()

    def _default_port(self, username: str) -> int:
        try:
            return self.base_port + (int(username) % 500)
        except ValueError:
            return self.base_port
