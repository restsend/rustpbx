"""RWI WebSocket client — mirrors the RustPBX RWI protocol.

Provides call control (originate, answer, hold, transfer…), queue operations,
conference operations, and a subscribe/event mechanism for real-time event
verification alongside webhook capture.
"""

from __future__ import annotations

import asyncio
import json
import logging
from typing import Any, Callable, Optional

import websockets

logger = logging.getLogger(__name__)


class RwiClient:
    """Async RWI WebSocket client."""

    def __init__(self, ws_url: str, token: str):
        self.ws_url = ws_url
        self.token = token
        self.ws: Optional[websockets.WebSocketClientProtocol] = None
        self._msg_id = 0
        self._pending: dict[str, asyncio.Future] = {}
        self._event_handlers: list[Callable[[dict], Any]] = []
        self._receive_task: Optional[asyncio.Task] = None
        self.events: list[dict] = []
        self.connected = False

    async def connect(self) -> None:
        url = f"{self.ws_url}?token={self.token}"
        self.ws = await websockets.connect(url)
        self.connected = True
        self._receive_task = asyncio.create_task(self._receive_loop())
        logger.info("RWI connected to %s", url)

    async def disconnect(self) -> None:
        self.connected = False
        if self._receive_task:
            self._receive_task.cancel()
            try:
                await self._receive_task
            except asyncio.CancelledError:
                pass
        if self.ws:
            await self.ws.close()

    async def _receive_loop(self) -> None:
        try:
            while True:
                try:
                    raw = await self.ws.recv()
                except websockets.exceptions.ConnectionClosed:
                    raise
                try:
                    data = json.loads(raw)
                except json.JSONDecodeError:
                    logger.warning("non-JSON RWI message: %s", raw[:200])
                    continue
                await self._handle(data)
        except websockets.exceptions.ConnectionClosed:
            logger.info("RWI WS closed")
        except asyncio.CancelledError:
            pass
        except Exception:
            logger.exception("RWI receive loop crashed")

    async def _handle(self, data: dict) -> None:
        action_id = data.get("action_id")
        if action_id and action_id in self._pending:
            fut = self._pending.pop(action_id)
            if not fut.done():
                fut.set_result(data)
            return
        event_type = data.get("event_type") or data.get("type")
        if event_type:
            self.events.append(data)
        for h in list(self._event_handlers):
            try:
                ret = h(data)
                if asyncio.iscoroutine(ret):
                    await ret
            except Exception:
                logger.exception("event handler error")

    def add_event_handler(self, handler: Callable[[dict], Any]) -> None:
        self._event_handlers.append(handler)

    def remove_event_handler(self, handler: Callable[[dict], Any]) -> None:
        if handler in self._event_handlers:
            self._event_handlers.remove(handler)

    def clear_events(self) -> None:
        self.events.clear()

    async def send_request(
        self, action: str, params: Optional[dict] = None, timeout: float = 10.0
    ) -> dict:
        self._msg_id += 1
        aid = f"py-{self._msg_id}"
        req = {"rwi": "1.0", "action_id": aid, "action": action, "params": params or {}}
        fut = asyncio.get_event_loop().create_future()
        self._pending[aid] = fut
        await self.ws.send(json.dumps(req))
        try:
            return await asyncio.wait_for(fut, timeout=timeout)
        except asyncio.TimeoutError:
            self._pending.pop(aid, None)
            raise TimeoutError(f"RWI request '{action}' timed out after {timeout}s")

    # ---- session ----

    async def subscribe(self, contexts: list[str], events: Optional[list[str]] = None) -> dict:
        params: dict = {"contexts": contexts}
        if events:
            params["events"] = events
        return await self.send_request("session.subscribe", params)

    async def list_calls(self) -> dict:
        return await self.send_request("session.list_calls")

    # ---- call control ----

    async def originate(
        self,
        call_id: str,
        destination: str,
        caller_id: Optional[str] = None,
        context: str = "default",
        timeout_secs: int = 30,
    ) -> dict:
        params: dict = {
            "call_id": call_id,
            "destination": destination,
            "context": context,
            "timeout_secs": timeout_secs,
        }
        if caller_id:
            params["caller_id"] = caller_id
        # The server's originate handler waits up to `timeout_secs` for the call
        # to be set up before replying, so the client request timeout must exceed
        # that window (send_request defaults to only 10s).
        return await self.send_request("call.originate", params, timeout=timeout_secs + 8)

    async def answer(self, call_id: str) -> dict:
        return await self.send_request("call.answer", {"call_id": call_id})

    async def hangup(self, call_id: str, reason: Optional[str] = None) -> dict:
        params: dict = {"call_id": call_id}
        if reason:
            params["reason"] = reason
        return await self.send_request("call.hangup", params)

    async def reject(self, call_id: str, reason: Optional[str] = None) -> dict:
        params: dict = {"call_id": call_id}
        if reason:
            params["reason"] = reason
        return await self.send_request("call.reject", params)

    async def ring(self, call_id: str) -> dict:
        return await self.send_request("call.ring", {"call_id": call_id})

    async def bridge(self, leg_a: str, leg_b: str) -> dict:
        return await self.send_request("call.bridge", {"leg_a": leg_a, "leg_b": leg_b})

    async def unbridge(self, call_id: str) -> dict:
        return await self.send_request("call.unbridge", {"call_id": call_id})

    async def hold(self, call_id: str) -> dict:
        return await self.send_request("call.hold", {"call_id": call_id})

    async def unhold(self, call_id: str) -> dict:
        return await self.send_request("call.unhold", {"call_id": call_id})

    async def transfer(
        self, call_id: str, target: str, attended: bool = False
    ) -> dict:
        params: dict = {"call_id": call_id, "target": target}
        if attended:
            params["attended"] = True
        return await self.send_request("call.transfer", params)

    async def transfer_attended(
        self, call_id: str, target: str, timeout_secs: Optional[int] = None
    ) -> dict:
        """Start an attended (consult) transfer: holds the caller leg and
        returns {original_call_id, consultation_call_id}. The client dials the
        consult target itself, then completes via :meth:`transfer_complete`."""
        params: dict = {"call_id": call_id, "target": target}
        if timeout_secs is not None:
            params["timeout_secs"] = timeout_secs
        return await self.send_request("call.transfer.attended", params)

    async def transfer_complete(
        self, call_id: str, consultation_call_id: str
    ) -> dict:
        return await self.send_request(
            "call.transfer.complete",
            {"call_id": call_id, "consultation_call_id": consultation_call_id},
        )

    async def transfer_cancel(self, consultation_call_id: str) -> dict:
        return await self.send_request(
            "call.transfer.cancel",
            {"consultation_call_id": consultation_call_id},
        )

    # ---- supervisor ----

    async def supervisor_listen(
        self, supervisor_call_id: str, target_call_id: str
    ) -> dict:
        return await self.send_request(
            "supervisor.listen",
            {"supervisor_call_id": supervisor_call_id, "target_call_id": target_call_id},
        )

    async def supervisor_whisper(
        self, supervisor_call_id: str, target_call_id: str, agent_leg: str
    ) -> dict:
        return await self.send_request(
            "supervisor.whisper",
            {
                "supervisor_call_id": supervisor_call_id,
                "target_call_id": target_call_id,
                "agent_leg": agent_leg,
            },
        )

    async def supervisor_barge(
        self, supervisor_call_id: str, target_call_id: str, agent_leg: str
    ) -> dict:
        return await self.send_request(
            "supervisor.barge",
            {
                "supervisor_call_id": supervisor_call_id,
                "target_call_id": target_call_id,
                "agent_leg": agent_leg,
            },
        )

    async def supervisor_takeover(
        self, supervisor_call_id: str, target_call_id: str
    ) -> dict:
        return await self.send_request(
            "supervisor.takeover",
            {"supervisor_call_id": supervisor_call_id, "target_call_id": target_call_id},
        )

    async def supervisor_stop(
        self, supervisor_call_id: str, target_call_id: str
    ) -> dict:
        return await self.send_request(
            "supervisor.stop",
            {"supervisor_call_id": supervisor_call_id, "target_call_id": target_call_id},
        )

    async def send_dtmf(self, call_id: str, digits: str, duration_ms: int = 100) -> dict:
        return await self.send_request(
            "call.send_dtmf",
            {"call_id": call_id, "digits": digits, "duration_ms": duration_ms},
        )

    async def get_call_info(self, call_id: str) -> dict:
        return await self.send_request("call.info", {"call_id": call_id})

    # ---- media ----

    async def media_play(
        self,
        call_id: str,
        source_type: str,
        uri: str,
        loop: bool = False,
        leg_id: Optional[str] = None,
        interrupt_on_dtmf: bool = False,
    ) -> dict:
        params: dict = {
            "call_id": call_id,
            "source": {"type": source_type, "uri": uri},
            "loop": loop,
        }
        if leg_id is not None:
            params["leg_id"] = leg_id
        if interrupt_on_dtmf:
            params["interrupt_on_dtmf"] = True
        return await self.send_request("media.play", params)

    async def media_stop(self, call_id: str) -> dict:
        return await self.send_request("media.stop", {"call_id": call_id})

    # ---- recording ----

    async def record_start(self, call_id: str, path: str, beep: bool = True) -> dict:
        return await self.send_request(
            "record.start",
            {"call_id": call_id, "storage": {"type": "file", "path": path}, "beep": beep},
        )

    async def record_stop(self, call_id: str) -> dict:
        return await self.send_request("record.stop", {"call_id": call_id})

    # ---- queue ----

    async def queue_enqueue(
        self, call_id: str, queue_id: str, priority: Optional[int] = None
    ) -> dict:
        params: dict = {"call_id": call_id, "queue_id": queue_id}
        if priority is not None:
            params["priority"] = priority
        return await self.send_request("queue.enqueue", params)

    async def queue_dequeue(self, call_id: str) -> dict:
        return await self.send_request("queue.dequeue", {"call_id": call_id})

    async def queue_agent_login(self, agent_id: str, queue_id: str) -> dict:
        return await self.send_request(
            "queue.agent_login", {"agent_id": agent_id, "queue_id": queue_id}
        )

    async def queue_agent_logout(self, agent_id: str) -> dict:
        return await self.send_request("queue.agent_logout", {"agent_id": agent_id})

    async def queue_agent_ready(self, agent_id: str, ready: bool = True) -> dict:
        return await self.send_request(
            "queue.agent_ready", {"agent_id": agent_id, "ready": ready}
        )

    async def queue_status(self, queue_id: str) -> dict:
        return await self.send_request("queue.status", {"queue_id": queue_id})

    # ---- conference ----

    async def conference_create(self, conf_id: str, max_members: Optional[int] = None) -> dict:
        params: dict = {"conf_id": conf_id}
        if max_members:
            params["max_members"] = max_members
        return await self.send_request("conference.create", params)

    async def conference_destroy(self, conf_id: str) -> dict:
        return await self.send_request("conference.destroy", {"conf_id": conf_id})

    async def conference_add(self, conf_id: str, call_id: str) -> dict:
        return await self.send_request(
            "conference.add", {"conf_id": conf_id, "call_id": call_id}
        )

    async def conference_remove(self, conf_id: str, call_id: str) -> dict:
        return await self.send_request(
            "conference.remove", {"conf_id": conf_id, "call_id": call_id}
        )

    async def conference_mute(self, conf_id: str, call_id: str) -> dict:
        return await self.send_request(
            "conference.mute", {"conf_id": conf_id, "call_id": call_id}
        )

    async def conference_unmute(self, conf_id: str, call_id: str) -> dict:
        return await self.send_request(
            "conference.unmute", {"conf_id": conf_id, "call_id": call_id}
        )

    # ---- event helpers ----

    async def wait_for_event(
        self, event_type: str, timeout: float = 10.0
    ) -> Optional[dict]:
        deadline = asyncio.get_event_loop().time() + timeout
        # Scan the FULL buffer: the event may have been enqueued BEFORE this
        # call (e.g. originate() already awaited past the answer), and a
        # start_len-based window would skip it and time out spuriously.
        while asyncio.get_event_loop().time() < deadline:
            for ev in self.events:
                et = ev.get("event_type") or ev.get("type")
                if et == event_type:
                    return ev
            await asyncio.sleep(0.1)
        return None

    async def wait_for_event_sequence(
        self, expected: list[str], timeout: float = 15.0
    ) -> bool:
        """Wait until *expected* event types appear in order."""
        deadline = asyncio.get_event_loop().time() + timeout
        idx = 0
        start_len = len(self.events)
        while asyncio.get_event_loop().time() < deadline:
            for ev in self.events[start_len:]:
                et = ev.get("event_type") or ev.get("type")
                if idx < len(expected) and et == expected[idx]:
                    idx += 1
                    if idx == len(expected):
                        return True
            await asyncio.sleep(0.1)
        return idx == len(expected)
