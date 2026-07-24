#!/usr/bin/env python3
"""
demo: ivr.exec — inject IVR during an active call via SIP INFO.

This script demonstrates how an external system (e.g., a CTI app, IVR provider,
or CLI tool) sends a SIP INFO with application/vnd.rustpbx+json to trigger an
IVR on the caller side while the agent (callee) is placed on hold.

Usage (conceptual — requires SIP dialog access):
  1. Establish a call through the PBX (e.g., caller=1001, agent=1000).
  2. Get the callee-side Dialog-ID from the PBX admin / event log.
  3. Run this script with the dialog info.

Prerequisites:
  - Python 3.10+
  - `pip install aiosip` (or equivalent SIP library)
"""

import json
import uuid
import asyncio

# ---------------------------------------------------------------------------
# SIP INFO command templates
# ---------------------------------------------------------------------------

RUSTPBX_CT = "application/vnd.rustpbx+json"


def make_ivr_exec(
    request_id: str | None = None,
    app: str = "ivr",
    ivr_params: dict | None = None,
    music: dict | None = None,
    hold_agent: bool = True,
    webhook_url: str | None = None,
    metadata: dict | None = None,
) -> bytes:
    """Build an ivr.exec SIP INFO body."""
    params: dict = {
        "request_id": request_id or str(uuid.uuid4()),
        "app": app,
    }
    if ivr_params:
        params["ivr_params"] = ivr_params
    if music:
        params["music"] = music
    params["hold_agent"] = hold_agent
    if webhook_url:
        params["webhook_url"] = webhook_url
    if metadata:
        params["metadata"] = metadata
    payload = {"action": "ivr.exec", "params": params}
    return json.dumps(payload, ensure_ascii=False).encode("utf-8")


def make_app_start(app_name: str = "ivr", app_params: dict | None = None) -> bytes:
    """Build an app.start SIP INFO body."""
    payload = {
        "action": "app.start",
        "params": {
            "app_name": app_name,
            "app_params": app_params or {},
        },
    }
    return json.dumps(payload, ensure_ascii=False).encode("utf-8")


def make_app_stop() -> bytes:
    """Build an app.stop SIP INFO body."""
    payload = {"action": "app.stop", "params": {}}
    return json.dumps(payload, ensure_ascii=False).encode("utf-8")


def make_hold(
    leg_id: str = "caller",
    music: dict | None = None,
) -> bytes:
    """Build a hold SIP INFO body with optional custom music."""
    params: dict = {"leg_id": leg_id}
    if music:
        params["music"] = music
    payload = {"action": "hold", "params": params}
    return json.dumps(payload, ensure_ascii=False).encode("utf-8")


# ---------------------------------------------------------------------------
# Usage examples
# ---------------------------------------------------------------------------

def demo_ivr_exec_survey():
    """Send a CSAT survey IVR to the caller while holding the agent."""
    body = make_ivr_exec(
        app="ivr",
        ivr_params={"file": "csat_survey.toml"},
        music={"source_type": "file", "uri": "sounds/hold_music.wav"},
        webhook_url="https://api.example.com/ivr-callback",
        metadata={"agent_id": "agent-001"},
    )
    print(f"Content-Type: {RUSTPBX_CT}")
    print(f"Body ({len(body)} bytes):\n{body.decode()}")


def demo_ivr_exec_timed():
    """Send an IVR with a specific request_id for result matching."""
    body = make_ivr_exec(
        request_id="my-correlator-001",
        app="ivr",
        ivr_params={"file": "collect_phone.toml"},
    )
    print(f"Sending ivr.exec with request_id=my-correlator-001")
    print(body.decode())


def demo_app_start_voicemail():
    """Start a voicemail app on the caller without holding the agent."""
    body = make_app_start(app_name="voicemail", app_params={"mailbox": "1000"})
    print(f"Content-Type: {RUSTPBX_CT}")
    print(body.decode())


def demo_hold_with_music():
    """Hold the caller with custom music."""
    body = make_hold(
        leg_id="caller",
        music={"source_type": "file", "uri": "sounds/premium_hold.wav"},
    )
    print(f"Content-Type: {RUSTPBX_CT}")
    print(body.decode())


if __name__ == "__main__":
    print("=" * 60)
    print("Demo 1: IVR exec — CSAT survey")
    print("=" * 60)
    demo_ivr_exec_survey()
    print()

    print("=" * 60)
    print("Demo 2: IVR exec — with custom request_id")
    print("=" * 60)
    demo_ivr_exec_timed()
    print()

    print("=" * 60)
    print("Demo 3: app.start voicemail (non-bundled)")
    print("=" * 60)
    demo_app_start_voicemail()
    print()

    print("=" * 60)
    print("Demo 4: hold with custom music")
    print("=" * 60)
    demo_hold_with_music()
