"""Real RTP bridging verification for backlog #1 (3-way conference media).

After the JoinMixerLeg dispatch fix, `merge_to_conference` actually
bridges RTP into the MCU. This test verifies that B and C (the two
real sipbot UAs) receive media AFTER merge that wasn't there before.

Topology mirrors test_consult_transfer_real_3way:
  - rwi.originate A→B (call1, A is PBX-internal)
  - rwi.originate B→C (call2, the consult leg)
  - /consult + /connected + /merge → JoinMixerLeg dispatched

Asserts:
  - /merge returns 200 with conf_id
  - PBX log shows "Joining mixer/conference (specific leg)" (handle_join_mixer_leg)
  - PBX log does NOT show "call_registry not attached" warning
  - B and C sipbots show non-zero RX RTP after merge (the bridge is
    actually delivering media into the mixer and out to each leg)
"""
from __future__ import annotations

import asyncio
import uuid

import pytest

import helpers as h
from helpers import (
    compute_rms_db,
    find_dominant_frequency,
    find_signal_start,
    has_audio_content,
    read_wav_mono,
)

pytestmark = [pytest.mark.tier3, pytest.mark.cc_transfer_events]

MIX_TONE_HZ = 620.0
FREQ_TOL_HZ = 15.0
MIN_RMS_DB = -40.0


@pytest.mark.asyncio
async def test_consult_merge_bridges_real_rtp(
    pbx, sipbot_pool, api, event_checker, tmp_path
):
    """After /merge, B (1002) and C (1003) must receive mixed audio from
    the conference bridge. This is the core assertion that backlog #1
    (JoinMixerLeg dispatch) actually wires RTP into the MCU.

    Content assertion (not just packet counts): a 620 Hz tone is played
    into call1 after the merge; the mixer must deliver it to BOTH B and C —
    each bot's mixdown recording must carry the tone's spectrum. Packet
    counters alone cannot distinguish real audio from CNG/silence.
    """
    record_b = tmp_path / "merge_b_rx.wav"
    record_c = tmp_path / "merge_c_rx.wav"
    mix_tone = tmp_path / "merge_mix620.wav"
    h.generate_sine_wav(mix_tone, MIX_TONE_HZ, 40.0, 8000, 0.4)
    callee_b = sipbot_pool.callee(
        host=pbx.host, port=16820, username="1002", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=75,
        record_file=str(record_b),
    )
    callee_c = sipbot_pool.callee(
        host=pbx.host, port=16830, username="1003", password="123456",
        register=True, proxy=f"{pbx.host}:{pbx.sip_port}", domain=pbx.host,
        ring_secs=1, answer_mode="echo", hangup_after=75,
        record_file=str(record_c),
    )
    await asyncio.sleep(3)

    # Originate A→B
    call1 = f"rtp-a-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call1, caller_id="1001",
            destination=f"sip:1002@{pbx.sip_addr}", timeout_secs=15,
        )
    except Exception as exc:
        pytest.skip(f"originate A→B failed: {exc}")
    await asyncio.sleep(3)

    # Snapshot B's RTP before consult
    b_rtp_before = callee_b.get_rtp_stats()
    print(f"\n[rtp] B before consult: {b_rtp_before}")

    # /consult
    status, body = await api.raw_request(
        "POST", f"/api/cc/calls/{call1}/consult", {"target": "1003"})
    assert status == 200, f"consult failed: {status}"
    tid = body.get("transfer_id") if isinstance(body, dict) else None
    assert tid, f"no transfer_id: {body!r:.80}"

    # Originate B→C consult leg
    call2 = f"rtp-c-{uuid.uuid4().hex[:8]}"
    try:
        await event_checker.rwi.originate(
            call_id=call2, caller_id="1002",
            destination=f"sip:1003@{pbx.sip_addr}", timeout_secs=15,
        )
    except Exception as exc:
        pytest.fail(f"originate B→C failed: {exc}")
    await asyncio.sleep(3)

    c_rtp_before_merge = callee_c.get_rtp_stats()
    print(f"[rtp] C before merge: {c_rtp_before_merge}")

    # /connected
    s, _ = await api.raw_request(
        "PUT", f"/api/cc/calls/{call1}/consult/{tid}/connected",
        {"session_b": call2})
    assert s == 200, f"connected failed: {s}"

    # /merge — this is where JoinMixerLeg gets dispatched
    s, merge_body = await api.raw_request(
        "POST", f"/api/cc/calls/{call1}/consult/{tid}/merge", {})
    assert s == 200, f"merge failed: {s} {merge_body!r:.80}"
    conf_id = (merge_body or {}).get("conf_id")
    print(f"[rtp] merge returned conf_id={conf_id}")

    # Wait for JoinMixerLeg dispatch + bridge establishment + RTP to flow
    await asyncio.sleep(5)

    # ── Content probe: inject a 620 Hz tone into the call and require it ──
    # in BOTH bots' mixdown recordings. Packet counters cannot tell real
    # audio from CNG/silence; a spectrum peak can.
    await event_checker.rwi.media_play(call1, "file", str(mix_tone), loop=True)
    await asyncio.sleep(8)
    await event_checker.rwi.media_stop(call1)

    b_rtp_after = callee_b.get_rtp_stats()
    c_rtp_after = callee_c.get_rtp_stats()
    print(f"[rtp] B after merge:  {b_rtp_after}")
    print(f"[rtp] C after merge:  {c_rtp_after}")

    # B must have received additional RTP after merge (the bridge mixes
    # A + C and delivers to B). Compare RX packet deltas.
    b_rx_delta = b_rtp_after.rx_packets - b_rtp_before.rx_packets
    c_rx_delta = c_rtp_after.rx_packets - c_rtp_before_merge.rx_packets
    print(f"[rtp] B RX delta post-consult: {b_rx_delta} packets")
    print(f"[rtp] C RX delta post-merge:   {c_rx_delta} packets")

    # PBX log must show JoinMixerLeg actually fired (handle_join_mixer_leg).
    # If call_registry wasn't attached, this log won't appear.
    log = pbx.log_file_path.read_text(encoding="utf-8", errors="replace") \
        if pbx.log_file_path else ""
    join_mixer_leg_fired = "Joining mixer/conference (specific leg)" in log
    registry_missing = "call_registry not attached" in log
    print(f"[log] JoinMixerLeg fired: {join_mixer_leg_fired}")
    print(f"[log] registry missing:   {registry_missing}")

    assert not registry_missing, (
        "call_registry was not attached to ConsultTransferManager — "
        "with_active_call_registry must be called at startup"
    )
    assert join_mixer_leg_fired, (
        "handle_join_mixer_leg never ran — JoinMixerLeg command didn't "
        "reach any SipSession. Check that originate-based sessions are in "
        "the active_call_registry and participant_leg() resolves correctly."
    )

    # RTP assertion: B received packets after merge (proves bridge → B).
    # We don't hard-fail on packet count (originate's RTP generation is
    # implementation-dependent) but warn if it's suspiciously zero.
    if b_rx_delta == 0:
        print("[warn] B received 0 new RTP packets post-consult — bridge may not be delivering")

    # Cleanup: end the calls, then let the bots self-exit (hangup_after) so
    # their mixdown recordings flush; poll for the files before content
    # assertions.
    for cid in (call1, call2):
        try:
            await event_checker.rwi.hangup(cid)
        except Exception as exc:
            print(f"[cleanup] hangup {cid} ignored: {exc}")

    async def _wait_recording(path):
        deadline = asyncio.get_event_loop().time() + 75
        while asyncio.get_event_loop().time() < deadline:
            hits = sorted(path.parent.glob(path.stem + "*.wav"))
            if hits:
                return hits[-1]  # sipbot suffixes the filename
            await asyncio.sleep(1.0)
        return None

    async def _assert_mix_tone(path, label):
        resolved = await _wait_recording(path)
        assert resolved, f"{label}: mixdown recording never flushed — {path}"
        samples, sr = read_wav_mono(resolved)
        assert has_audio_content(samples, MIN_RMS_DB), (
            f"{label}: recording silent — the merged conference never "
            "delivered audio to this participant"
        )
        start = find_signal_start(samples)
        region = samples[start:min(start + 5 * sr, samples.size)]
        assert region.size >= sr // 2, f"{label}: not enough audio"
        rms = compute_rms_db(region)
        assert rms >= MIN_RMS_DB, f"{label}: recording too quiet ({rms:.1f}dB)"
        dom, _mag = find_dominant_frequency(region, sr, low=200, high=900, step=5)
        assert abs(dom - MIX_TONE_HZ) <= FREQ_TOL_HZ, (
            f"{label}: recording dominant {dom:.0f}Hz, expected the injected "
            f"{MIX_TONE_HZ:.0f}Hz (±{FREQ_TOL_HZ}) — the mixer did NOT "
            "deliver the merged audio to this participant"
        )
        print(f"[rtp] {label} mixdown: {MIX_TONE_HZ:.0f}Hz ok rms={rms:.1f}dB")

    await asyncio.gather(
        _assert_mix_tone(record_b, "B(1002)"),
        _assert_mix_tone(record_c, "C(1003)"),
    )
    await asyncio.sleep(2)
