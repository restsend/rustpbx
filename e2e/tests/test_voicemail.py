"""Voicemail addon E2E tests.

Configures a route to the voicemail app, has a caller leave a message (DTMF '#'
to finish), and verifies a recording is persisted.
"""

from __future__ import annotations

import asyncio
import uuid
from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.voicemail]


@pytest.mark.xfail(reason="WIP media layer: voicemail recording not persisted under WebRTC")
@pytest.mark.asyncio
async def test_voicemail_records_message(pbx, sipbot_pool, tmp_path):
    """Caller -> route app=voicemail -> records greeting, leaves message, DTMF '#' ends."""
    storage = tmp_path / "vm_recordings"
    pbx.config_builder.add_voicemail(
        spool_dir=str(tmp_path / "spool"),
        storage_path=str(storage),
        max_duration_secs=60,
    )
    pbx.config_builder.add_route(
        "to-vm",
        match={"to.user": "vm"},
        priority=10,
        action="application",
        app="voicemail",
        app_params={"extension": "1001"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:vm@{pbx.sip_addr}", username="1001", password="123456",
        hangup=12, dtmf_flows="5s:#", audio_quality=True,
    )
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output

    # Voicemail answers, plays greeting, records; '#' at 5s stops recording + hangs up.
    deadline = asyncio.get_event_loop().time() + 25
    recordings: list[Path] = []
    while asyncio.get_event_loop().time() < deadline:
        recordings = list(storage.rglob("*.wav"))
        if recordings:
            break
        await asyncio.sleep(0.5)
    assert recordings, f"no voicemail recording under {storage}"
    assert recordings[0].stat().st_size > 44, "voicemail recording is empty"

    # Caller received the greeting audio (non-silent).
    await h.wait_rtp(caller, "caller", 15)
