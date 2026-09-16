"""Assertion engine for RWI webhook and WebSocket events.

Provides high-level assertion functions that integrate with pytest:
  - expect_event: wait for a single event type
  - expect_sequence: wait for an ordered subsequence of events
  - expect_no_event: assert an event does NOT arrive within a window
  - assert_rtp_bidirectional: assert sipbot reports bidirectional RTP
"""

from __future__ import annotations

import asyncio
import logging
from functools import wraps
from typing import Optional

import pytest

from .webhook_receiver import WebhookReceiver, _dig
from .rwi_client import RwiClient
from .sipbot import RtpStats


def _dig_event(ev, path: str):
    """Resolve a dotted path against a WebhookEvent.

    Digs through the event object itself (attribute access), so the
    documented convention ``payload.<field>`` resolves via ``ev.payload``
    — the event-fields dict — exactly like the ``match`` filter in
    ``WebhookReceiver.wait_for_event``. Digging ``ev.raw`` instead would
    resolve against the webhook envelope (whose key is ``event``, not
    ``payload``) and always yield None.
    """
    return _dig(ev, path)

logger = logging.getLogger(__name__)


def skip_if_auth_needed(func):
    """Decorator: skip test if CC REST API returns 401 (needs PhoneAuth JWT)."""
    from functools import wraps

    @wraps(func)
    async def wrapper(*args, **kwargs):
        try:
            return await func(*args, **kwargs)
        except Exception as exc:
            msg = str(exc)
            if "401" in msg or "invalid or expired token" in msg:
                pytest.skip("CC REST API requires PhoneAuth JWT authentication")
            raise

    return wrapper


class EventChecker:
    """Wraps webhook + RWI WS events for assertion-style verification."""

    def __init__(
        self,
        webhook: Optional[WebhookReceiver] = None,
        rwi: Optional[RwiClient] = None,
    ):
        self.webhook = webhook
        self.rwi = rwi

    async def expect_webhook_event(
        self,
        event_type: str,
        *,
        timeout: float = 15.0,
        call_id: Optional[str] = None,
        occurrence: int = 1,
        match: Optional[dict] = None,
    ):
        """Wait for a webhook event and FAIL the test if it never arrives.

        This is the strict counterpart of ``webhook.wait_for_event`` (which
        returns None on timeout). Always prefer this helper in assertions —
        a bare ``wait_for_event`` whose return value is ignored is a silent
        pass and is flagged by ``scripts/check_assertion_quality.py``.

        ``match`` maps dotted payload paths to expected values, e.g.
        ``{"payload.agent_id": "1003"}``; ``occurrence`` selects the Nth
        matching event (1-based).
        """
        assert self.webhook is not None, "webhook receiver not configured"
        ev = await self.webhook.wait_for_event(
            event_type,
            timeout=timeout,
            call_id=call_id,
            occurrence=occurrence,
            match=match,
        )
        if ev is None:
            detail = f"event='{event_type}'"
            if call_id is not None:
                detail += f" call_id={call_id!r}"
            if match:
                detail += f" match={match!r}"
            if occurrence > 1:
                detail += f" occurrence={occurrence}"
            pytest.fail(
                f"Expected webhook event within {timeout}s: {detail}. "
                f"Got events: {self.webhook.event_types()}"
            )
        return ev

    async def expect_webhook_payload(
        self,
        event_type: str,
        match: dict,
        *,
        timeout: float = 15.0,
        call_id: Optional[str] = None,
        occurrence: int = 1,
    ):
        """Strict wait + per-field payload verification.

        Asserts the event arrived AND every ``match`` path equals the
        expected value (defense in depth: ``wait_for_event`` already
        filters, this makes the payload contract explicit in the failure
        message).
        """
        ev = await self.expect_webhook_event(
            event_type,
            timeout=timeout,
            call_id=call_id,
            occurrence=occurrence,
            match=match,
        )
        for path, expected in match.items():
            actual = ev.payload if path == "payload" else _dig_event(ev, path)
            assert actual == expected, (
                f"{event_type} payload mismatch at '{path}': "
                f"expected {expected!r}, got {actual!r} (event: {ev.raw!r:.400})"
            )
        return ev

    async def expect_webhook_sequence(
        self,
        expected: list[str],
        *,
        timeout: float = 30.0,
        call_id: Optional[str] = None,
    ) -> None:
        assert self.webhook is not None, "webhook receiver not configured"
        ok = await self.webhook.wait_for_sequence(
            expected, timeout=timeout, call_id=call_id
        )
        if not ok:
            pytest.fail(
                f"Expected webhook sequence {expected} within {timeout}s. "
                f"Got: {self.webhook.event_types_for_call(call_id) if call_id else self.webhook.event_types()}"
            )

    async def expect_no_webhook_event(
        self, event_type: str, *, wait: float = 3.0
    ) -> None:
        assert self.webhook is not None
        ev = await self.webhook.wait_for_event(event_type, timeout=wait)
        if ev is not None:
            pytest.fail(f"Unexpected webhook event '{event_type}' arrived: {ev}")

    async def expect_rwi_event(
        self, event_type: str, *, timeout: float = 15.0
    ):
        assert self.rwi is not None, "RWI client not configured"
        ev = await self.rwi.wait_for_event(event_type, timeout=timeout)
        if ev is None:
            pytest.fail(
                f"Expected RWI event '{event_type}' within {timeout}s. "
                f"Got: {[e.get('event_type') for e in self.rwi.events]}"
            )
        return ev

    async def expect_rwi_sequence(
        self, expected: list[str], *, timeout: float = 15.0
    ) -> None:
        assert self.rwi is not None
        ok = await self.rwi.wait_for_event_sequence(expected, timeout=timeout)
        if not ok:
            pytest.fail(
                f"Expected RWI sequence {expected} within {timeout}s. "
                f"Got: {[e.get('event_type') for e in self.rwi.events]}"
            )

    async def expect_min_webhook_events(
        self, count: int, *, timeout: float = 15.0
    ) -> None:
        assert self.webhook is not None
        ok = await self.webhook.wait_for_min_events(count, timeout=timeout)
        if not ok:
            pytest.fail(
                f"Expected at least {count} webhook events within {timeout}s. "
                f"Got {self.webhook.count()}"
            )

    # ------------------------------------------------------------------
    # RWI WS ↔ webhook one-to-one correlation
    # ------------------------------------------------------------------

    def rwi_events_for_call(self, call_id: str) -> list[dict]:
        """RWI WebSocket events whose call_id matches."""
        assert self.rwi is not None, "RWI client not configured"
        out = []
        for e in self.rwi.events:
            if (e.get("call_id") or e.get("callId")) == call_id:
                out.append(e)
        return out

    def rwi_event_types_for_call(self, call_id: str) -> list[str]:
        return [e.get("event_type") or e.get("type") or "" for e in self.rwi_events_for_call(call_id)]

    def webhook_event_types_for_call(self, call_id: str) -> list[str]:
        assert self.webhook is not None
        return self.webhook.event_types_for_call(call_id)

    def assert_event_correlation(
        self,
        call_id: str,
        expected: list[str],
        *,
        channel: str = "both",
    ) -> None:
        """Assert `expected` event types appear for `call_id` in the requested
        channel(s), proving the event flowed through RWI WS AND/OR webhook.

        channel: "both" (default) = each expected type must appear in BOTH;
                 "webhook" = webhook only; "rwi" = RWI WS only.
        """
        wh = self.webhook_event_types_for_call(call_id) if self.webhook else []
        rwi = self.rwi_event_types_for_call(call_id) if self.rwi else []
        missing_wh = [t for t in expected if t not in wh] if self.webhook else []
        missing_rwi = [t for t in expected if t not in rwi] if self.rwi else []
        if channel in ("both", "webhook") and missing_wh:
            pytest.fail(
                f"webhook missing events {missing_wh} for call {call_id}. "
                f"webhook had: {wh}"
            )
        if channel in ("both", "rwi") and missing_rwi:
            pytest.fail(
                f"RWI WS missing events {missing_rwi} for call {call_id}. "
                f"rwi had: {rwi}"
            )

    def assert_correlation_symmetric(self, call_id: str, *, ignore: Optional[list[str]] = None) -> None:
        """Assert call-lifecycle event types agree across RWI WS and webhook
        for `call_id` (symmetric set membership, ignoring non-call broadcast
        events and any types in `ignore`)."""
        ignore = set(ignore or [])
        wh = {t for t in self.webhook_event_types_for_call(call_id) if t and t not in ignore}
        rwi = {t for t in self.rwi_event_types_for_call(call_id) if t and t not in ignore}
        only_wh = wh - rwi
        only_rwi = rwi - wh
        if only_wh or only_rwi:
            pytest.fail(
                f"RWI↔webhook event mismatch for call {call_id}:\n"
                f"  webhook-only: {sorted(only_wh)}\n"
                f"  rwi-only:     {sorted(only_rwi)}\n"
                f"  common:       {sorted(wh & rwi)}"
            )

    @staticmethod
    def assert_rtp_bidirectional(
        stats: RtpStats, *, min_packets: int = 50, label: str = ""
    ) -> None:
        prefix = f"[{label}] " if label else ""
        assert stats.rx_packets >= min_packets, (
            f"{prefix}RTP RX packets too low: {stats.rx_packets} < {min_packets}"
        )
        assert stats.tx_packets >= min_packets, (
            f"{prefix}RTP TX packets too low: {stats.tx_packets} < {min_packets}"
        )

    @staticmethod
    def assert_rtp_rx_only(
        stats: RtpStats, *, min_packets: int = 50, label: str = ""
    ) -> None:
        prefix = f"[{label}] " if label else ""
        assert stats.rx_packets >= min_packets, (
            f"{prefix}RTP RX packets too low: {stats.rx_packets} < {min_packets}"
        )

    @staticmethod
    def assert_sip_answered(output: str, *, label: str = "") -> None:
        prefix = f"[{label}] " if label else ""
        assert "200 OK" in output or "Call established" in output, (
            f"{prefix}SIP call not established. Output: {output[-500:]}"
        )

    @staticmethod
    def assert_sip_rejected(output: str, *, label: str = "") -> None:
        prefix = f"[{label}] " if label else ""
        codes = ["486", "487", "603", "404", "480", "403", "503"]
        assert any(c in output for c in codes), (
            f"{prefix}Expected SIP rejection. Output: {output[-500:]}"
        )
