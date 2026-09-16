"""Full-JSON RWI event-hook flow capture for the regression report.

Generalizes the ``_dump_event_timeline`` helper used by the flow1/flow2 e2e
tests: for every test we dump the COMPLETE raw JSON payloads of both event
channels that make up the "RWI event hook" flow:

  - ``webhook``  : the raw request body rustpbx POSTed to the RWI webhook
                   receiver (verbatim, per flow1).
  - ``rwi_ws``   : the raw event dicts received on the RWI WebSocket, in
                   arrival order.

Each test writes ``report/flows/<test_id>.json`` and the HTML report links it
and renders the payload inline so a user can inspect exactly which JSON events
a given test flow produced.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Optional

from .webhook_receiver import WebhookEvent


def _sanitize(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", name)


class EventFlowCapture:
    """Captures + persists one test's complete event-hook JSON flow."""

    def __init__(self, report_dir: Path):
        self.report_dir = Path(report_dir)
        self.flows_dir = self.report_dir / "flows"

    def capture(
        self,
        *,
        test_id: str,
        webhook_events: Optional[list[WebhookEvent]] = None,
        rwi_events: Optional[list[dict]] = None,
        meta: Optional[dict] = None,
        overwrite: bool = True,
    ) -> dict:
        """Build + write the merged flow; return the full dict with ``path``."""
        self.flows_dir.mkdir(parents=True, exist_ok=True)
        webhook = self._webhook_section(list(webhook_events or []))
        rwi = self._rwi_section(list(rwi_events or []))
        flow: dict[str, Any] = {
            "test": test_id,
            **(meta or {}),
            "webhook_count": len(webhook),
            "rwi_count": len(rwi),
            "webhook": webhook,
            "rwi_ws": rwi,
        }
        safe = _sanitize(test_id)
        path = self.flows_dir / f"{safe}.json"
        if overwrite or not path.exists():
            path.write_text(
                json.dumps(flow, indent=2, ensure_ascii=False), encoding="utf-8"
            )
        return {**flow, "path": f"flows/{safe}.json"}

    # ---- sections ----

    def _webhook_section(self, events: list[WebhookEvent]) -> list[dict]:
        evs = sorted(events, key=lambda e: e.timestamp)
        if not evs:
            return []
        base = evs[0].timestamp
        out = []
        for i, ev in enumerate(evs):
            out.append(
                {
                    "seq": i,
                    "t_offset_s": round(ev.timestamp - base, 3),
                    "received_at": ev.timestamp,
                    "sequence": ev.sequence,
                    "event_type": ev.event_type,
                    "call_id": ev.call_id,
                    "raw": ev.raw,
                }
            )
        return out

    def _rwi_section(self, events: list[dict]) -> list[dict]:
        out = []
        for i, ev in enumerate(events):
            out.append(
                {
                    "seq": i,
                    "event_type": ev.get("event_type") or ev.get("type"),
                    "call_id": ev.get("call_id") or ev.get("callId"),
                    "raw": ev,
                }
            )
        return out
