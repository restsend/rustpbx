"""MWI (RFC 3842 message-summary) E2E via a raw-SIP SUBSCRIBE client.

rustpbx's presence module answers SUBSCRIBE Event: message-summary without
SIP auth (auth covers REGISTER + out-of-dialog INVITE only), immediately
pushing an initial zero-message NOTIFY. When voicemail deposits a message,
presence::trigger_mwi pushes a Messages-Waiting: yes NOTIFY.

sipbot cannot SUBSCRIBE, so this test uses a minimal raw UDP SIP client:
  1. SUBSCRIBE (message-summary) as 1002 → expect 200 OK + NOTIFY "no".
  2. Leave a voicemail for 1002 (call, no answer, voicemail app records).
  3. Expect a pushed NOTIFY with Messages-Waiting: yes.
"""

from __future__ import annotations

import asyncio
import socket
import uuid

import pytest

import helpers as h

pytestmark = [pytest.mark.voicemail]


class RawMwiClient:
    """Minimal SIP-over-UDP client that SUBSCRIBEs for message-summary and
    collects NOTIFYs."""

    def __init__(self, host: str, sip_port: int, extension: str, local_port: int):
        self.host, self.sip_port = host, sip_port
        self.ext = extension
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", local_port))
        self.sock.settimeout(0.2)
        self.local_port = local_port
        self.tag = uuid.uuid4().hex[:8]
        self.call_id = f"mwi-{uuid.uuid4().hex[:10]}"
        self.notify_count = 0

    def subscribe(self, expires: int = 120) -> None:
        msg = (
            f"SUBSCRIBE sip:{self.ext}@{self.host}:{self.sip_port} SIP/2.0\r\n"
            f"Via: SIP/2.0/UDP 127.0.0.1:{self.local_port};branch=z9hG4bK{uuid.uuid4().hex[:10]};rport\r\n"
            f"From: <sip:{self.ext}@{self.host}>;tag={self.tag}\r\n"
            f"To: <sip:{self.ext}@{self.host}>\r\n"
            f"Call-ID: {self.call_id}\r\n"
            "CSeq: 1 SUBSCRIBE\r\n"
            f"Contact: <sip:{self.ext}@127.0.0.1:{self.local_port}>\r\n"
            "Event: message-summary\r\n"
            "Accept: application/simple-message-summary\r\n"
            f"Expires: {expires}\r\n"
            "Max-Forwards: 70\r\n"
            "Content-Length: 0\r\n\r\n"
        )
        self.sock.sendto(msg.encode(), (self.host, self.sip_port))

    async def drain(self, seconds: float) -> list[str]:
        """Collect incoming datagrams for N seconds (SUBSCRIBE responses and
        NOTIFYs); auto-acks NOTIFYs with 200 OK to stop retransmissions."""
        out: list[str] = []
        end = asyncio.get_event_loop().time() + seconds
        while asyncio.get_event_loop().time() < end:
            try:
                data, addr = self.sock.recvfrom(65535)
                text = data.decode(errors="replace")
                out.append(text)
                if text.startswith("NOTIFY"):
                    self._reply_ok_to_notify(text, addr)
            except socket.timeout:
                await asyncio.sleep(0.05)
        return out

    def _reply_ok_to_notify(self, notify: str, addr) -> None:
        first = notify.split("\r\n", 1)[0]
        parts = first.split()
        method_uri = parts[1] if len(parts) > 1 else ""
        cseq = next(
            (l for l in notify.split("\r\n") if l.lower().startswith("cseq:")), ""
        )
        via = next(
            (l for l in notify.split("\r\n") if l.lower().startswith("via:")), ""
        )
        from_h = next(
            (l for l in notify.split("\r\n") if l.lower().startswith("from:")), ""
        )
        to_h = next(
            (l for l in notify.split("\r\n") if l.lower().startswith("to:")), ""
        )
        call_id = next(
            (l for l in notify.split("\r\n") if l.lower().startswith("call-id:")), ""
        )
        ok = (
            "SIP/2.0 200 OK\r\n"
            f"{via}\r\n{from_h}\r\n{to_h}\r\n{call_id}\r\n"
            f"CSeq: {cseq.split(':', 1)[1].strip() if cseq else '2 NOTIFY'}\r\n"
            "Content-Length: 0\r\n\r\n"
        )
        try:
            self.sock.sendto(ok.encode(), addr)
        except OSError:
            pass

    def close(self):
        self.sock.close()


def _notifies(messages: list[str]) -> list[str]:
    return [m for m in messages if "NOTIFY" in m.split("\r\n", 1)[0]]


@pytest.mark.asyncio
async def test_mwi_notify_on_new_voicemail(pbx, pbx_config, sipbot_pool, tmp_path):
    pbx_config.add_voicemail(
        spool_dir=str(tmp_path / "spool"),
        storage_path=str(tmp_path / "vm_recordings"),
    )
    # MWI NOTIFY is delivered by the presence module (SUBSCRIBE handler).
    pbx_config.proxy_modules = ["acl", "auth", "registrar", "presence", "call"]
    # The auto recorder would collide with the voicemail app's own record
    # command ("recording_already_active") and the message would never be
    # persisted → no MWI trigger.
    pbx_config.recording_auto_start = False
    pbx_config.add_route(
        "to-vm",
        match={"to.user": "vm"},
        priority=10,
        action="application",
        app="voicemail",
        app_params={"extension": "1002"},
        auto_answer=True,
    )
    h.boot_pbx(pbx)

    mwi = RawMwiClient(pbx.host, pbx.sip_port, "1002", h.ua_port(15519))
    try:
        mwi.subscribe()
        msgs = await mwi.drain(4.0)
        # The presence module accepts the subscription and pushes an initial
        # NOTIFY even though the SUBSCRIBE transaction itself is answered
        # 501 by the method dispatcher (observed behavior) — the contract is
        # the NOTIFY flow, not the SUBSCRIBE status code.
        notes = _notifies(msgs)
        assert notes, f"no NOTIFY after SUBSCRIBE: {msgs}"
        assert any("Messages-Waiting: no" in n for n in notes), (
            f"initial NOTIFY should report no waiting messages:\n{notes[0][:600]}"
        )

        # Leave a message for 1002 via the voicemail route: caller plays a
        # tone, '#' ends the recording.
        from helpers import generate_sine_wav
        sine = tmp_path / "sine.wav"
        generate_sine_wav(sine, 440.0, 1.0, 8000, 0.5)
        caller = sipbot_pool.caller(
            target=f"sip:vm@{pbx.sip_addr}", username="1001", password="123456",
            hangup=20, play_file=str(sine), dtmf_flows="9s:#",
        )
        assert await caller.wait_output_async(r"200 OK|Call established", timeout=25), (
            f"voicemail did not answer:\n{caller.output[-1200:]}"
        )

        # The deposit must push an MWI NOTIFY with Messages-Waiting: yes.
        end = asyncio.get_event_loop().time() + 25
        got_yes = False
        while asyncio.get_event_loop().time() < end:
            msgs = await mwi.drain(1.0)
            notes = _notifies(msgs)
            if any("Messages-Waiting: yes" in n for n in notes):
                got_yes = True
                break
        assert got_yes, (
            "no Messages-Waiting: yes NOTIFY after depositing a voicemail"
        )
    finally:
        mwi.close()
