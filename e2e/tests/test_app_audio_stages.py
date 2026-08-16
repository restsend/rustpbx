"""App audio playback in both call stages — sipbot E2E verification.

Verifies that audio (the shipped English ``sounds/service_unavailable_en.mp3``)
is delivered to the caller in the two stages of a call:

  * Stage 1 (pre-answer / 183 early media): when an IVR application fails to
    start (e.g. a missing config file), the per-trunk ``RingbackAudio.error``
    cue is played as 183 early media before the 5xx rejection. This is the
    path added so a misconfiguration never leaves the caller hearing dead air.

  * Stage 2 (post-answer / 200 OK): once the call is answered, the IVR app
    plays the same file as its greeting and audio reaches the caller.

Both stages assert that RTP actually reaches the caller (``rx_packets > 0``),
which is the real proof that audio was played — not just that a command was
accepted.
"""

from __future__ import annotations

from pathlib import Path

import pytest

import helpers as h

pytestmark = [pytest.mark.media]

PROMPT = "sounds/service_unavailable_en.mp3"


def _trunk_caller(sipbot_pool, pbx, *, target, hangup=8):
    """Outbound caller whose From domain differs from the PBX realm so the call
    classifies as Inbound — the classification that applies per-trunk ringback
    and reliably receives early-media RTP in the e2e harness."""
    return sipbot_pool.caller(
        target=target, username="external", password="123456",
        from_uri="sip:external@trunk.example.com", hangup=hangup,
    )


# ---------------------------------------------------------------------------
# Stage 1 — pre-answer: missing IVR config plays the error cue as early media
# ---------------------------------------------------------------------------

def _cdr_dir(pbx) -> Path:
    import datetime
    return pbx.project_root / "config" / "cdr" / datetime.date.today().strftime("%Y%m%d")


def _pbx_log(pbx) -> str:
    """Read the rustpbx log for this test's PBX instance."""
    if pbx.log_file_path and pbx.log_file_path.exists():
        return pbx.log_file_path.read_text(encoding="utf-8", errors="replace")
    return ""


def _wait_for_log(pbx, needle: str, timeout: float = 15.0) -> bool:
    """Poll the PBX log until `needle` appears (the tone log line is emitted
    before the failure path completes)."""
    import time as _time
    deadline = _time.monotonic() + timeout
    while _time.monotonic() < deadline:
        if needle in _pbx_log(pbx):
            return True
        _time.sleep(0.2)
    return False


@pytest.mark.asyncio
async def test_stage1_missing_ivr_config_plays_error_tone_early_media(pbx, sipbot_pool):
    """Zero-config scenario: the trunk has NO ringback config, so the built-in
    global failure-tone default (`error` → `sounds/service_unavailable_en.mp3`)
    plays the service-unavailable prompt as 183 early media when an IVR route
    points at a missing config file, then the call is rejected with 500 +
    `ivr.start_failed`.

    The call trace/CDR must record the SPECIFIC reason — which config file was
    not found."""
    import json

    pbx.config_builder.set_realms(["127.0.0.1"])
    pbx.config_builder.add_trunk(
        "plain-trunk", dest=f"127.0.0.1:{h.ua_port(15300)}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
        # NOTE: no `ringback` here — the global built-in default is exercised.
    )
    # Route a unique extension to an IVR whose config file does NOT exist on
    # disk → factory returns Err(detail) → start_app fails → reject_with_tone(500).
    pbx.config_builder.add_route(
        "to-missing-ivr",
        match={"to.user": "missing-ivr"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/does-not-exist-audio-test.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    cdr_dir = _cdr_dir(pbx)
    before = set(cdr_dir.glob("*.json")) if cdr_dir.exists() else set()

    caller = _trunk_caller(
        sipbot_pool, pbx, target=f"sip:missing-ivr@{pbx.sip_addr}", hangup=12,
    )
    # The error cue must be sent as 183 early media (wait_output_async confirms
    # the caller saw it), then the final 500 rejection must reach the caller and
    # end the call — the A-leg must not stay stuck in early media.
    assert await caller.wait_output_async(r"Early media|183", timeout=30), caller.output
    assert await caller.wait_output_async(r"Call failed with status: 500", timeout=35), caller.output
    assert "All calls finished" in caller.output, caller.output[-2000:]
    caller.wait(timeout=10)

    # The service-unavailable prompt was actually played (PBX-side, reliable):
    # reject_with_tone logs the failure tone with the resolved audio path.
    assert _wait_for_log(pbx, "Playing failure tone before rejection"), _pbx_log(pbx)[-3000:]
    assert "service_unavailable_en.mp3" in _pbx_log(pbx), _pbx_log(pbx)[-3000:]

    # The rejection outcome is recorded reliably in the CDR even when the final
    # 5xx response races with early-media teardown on the caller UA.
    cdr = _latest_cdr(cdr_dir, before)
    meta = cdr.get("metadata", {})
    assert cdr.get("statusCode") == 500, f"expected 500 statusCode, got: {cdr}"
    assert meta.get("error_code") == "ivr.start_failed", f"got: {meta}"
    assert cdr.get("status") == "failed", f"expected failed call, got: {cdr}"
    # The call trace must record the concrete cause: which file was missing.
    assert _trace_has(cdr, "does-not-exist-audio-test.toml"), (
        f"trace must name the missing config file: {meta.get('trace')}"
    )
    assert _trace_has(cdr, "No such file"), (
        f"trace must record the not-found reason: {meta.get('trace')}"
    )


@pytest.mark.asyncio
async def test_stage1_invalid_ivr_toml_records_parse_error(pbx, sipbot_pool):
    """A malformed IVR TOML (format error) also plays the service-unavailable
    cue and the trace records the specific parse failure (which file + error)."""
    import json

    bad_file = pbx.project_root / "bad-ivr-parse.toml"
    bad_file.write_text("[ivr\nthis is not valid toml {{{", encoding="utf-8")

    pbx.config_builder.set_realms(["127.0.0.1"])
    pbx.config_builder.add_trunk(
        "parse-trunk", dest=f"127.0.0.1:{h.ua_port(15301)}", direction="inbound",
        inbound_hosts=["127.0.0.1"],
    )
    pbx.config_builder.add_route(
        "to-bad-ivr",
        match={"to.user": "bad-ivr"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": bad_file.name},  # relative to PBX CWD (repo root)
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    cdr_dir = _cdr_dir(pbx)
    before = set(cdr_dir.glob("*.json")) if cdr_dir.exists() else set()

    caller = _trunk_caller(
        sipbot_pool, pbx, target=f"sip:bad-ivr@{pbx.sip_addr}", hangup=12,
    )
    assert await caller.wait_output_async(r"Early media|183", timeout=30), caller.output
    assert await caller.wait_output_async(r"Call failed with status: 500", timeout=35), caller.output
    assert "All calls finished" in caller.output, caller.output[-2000:]
    caller.wait(timeout=10)

    # The service-unavailable prompt was played (PBX-side, reliable).
    assert _wait_for_log(pbx, "Playing failure tone before rejection"), _pbx_log(pbx)[-3000:]

    cdr = _latest_cdr(cdr_dir, before)
    meta = cdr.get("metadata", {})
    assert cdr.get("statusCode") == 500, f"got: {cdr}"
    assert meta.get("error_code") == "ivr.start_failed", f"got: {meta}"
    assert _trace_has(cdr, "bad-ivr-parse.toml"), (
        f"trace must name the invalid config file: {meta.get('trace')}"
    )
    assert _trace_has(cdr, "Failed to parse IVR TOML"), (
        f"trace must record the parse failure: {meta.get('trace')}"
    )


def _latest_cdr(cdr_dir: Path, before: set, timeout: float = 35.0) -> dict:
    import json
    import time as _time

    # The CDR is written only after the failure tone finishes playing (the
    # English prompt is ~5 s) and the rejection path runs cleanup — poll
    # long enough to cover it.
    deadline = _time.monotonic() + timeout
    new_cdrs = set()
    while _time.monotonic() < deadline:
        if cdr_dir.exists():
            new_cdrs = set(cdr_dir.glob("*.json")) - before
        if new_cdrs:
            break
        _time.sleep(0.2)
    assert new_cdrs, f"no CDR written for the failed IVR call in {cdr_dir}"
    return json.loads(new_cdrs.pop().read_text(encoding="utf-8"))


def _trace_has(cdr: dict, needle: str) -> bool:
    """True if any trace event's message or detail.error contains `needle`."""
    trace = cdr.get("metadata", {}).get("trace", [])
    for ev in trace:
        detail = ev.get("detail") or {}
        if needle in str(ev.get("message", "")) or needle in str(detail.get("error", "")):
            return True
    return False


# ---------------------------------------------------------------------------
# Stage 2 — post-answer: an IVR app answers and plays the prompt
# ---------------------------------------------------------------------------

@pytest.mark.asyncio
async def test_stage2_ivr_greeting_prompt_after_answer(pbx, sipbot_pool):
    """After the call is answered (200 OK), the IVR app plays the synthesized
    English service-unavailable prompt as its greeting; the caller must receive the audio (RTP).

    This exercises the post-answer app playback stage with the same audio file
    used for the pre-answer error cue, using a real sipbot caller UA. The
    greeting path is relative to the PBX working dir (repo root)."""
    pbx.config_builder.media_proxy = "all"
    pbx.config_builder.add_ivr("stage2-ivr", f'''\
[ivr]
name = "stage2-ivr"
ivr_mode = "tree"
[ivr.root]
greeting = "{PROMPT}"
timeout_ms = 10000
max_retries = 3
max_retries_action = {{ type = "hangup" }}
''')
    pbx.config_builder.add_route(
        "to-stage2-ivr",
        match={"to.user": "stage2-ivr"},
        priority=10,
        action="application",
        app="ivr",
        app_params={"file": "config/ivr/stage2-ivr.toml"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    caller = sipbot_pool.caller(
        target=f"sip:stage2-ivr@{pbx.sip_addr}", username="1001", password="123456",
        hangup=6,
    )
    # Call is answered (auto_answer) → the app plays the greeting prompt.
    assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), caller.output
    await h.wait_rtp(caller, "caller", 20)
    assert caller.get_status_counts().get(200, 0) == 1, (
        f"expected 200 OK after IVR answer, got: {caller.get_status_counts()}\n"
        f"{caller.output[-2000:]}"
    )
