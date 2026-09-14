#!/usr/bin/env python3
"""OpenAI Realtime smoke test — real provider, real API key.

Prerequisites:
  * a running rustpbx with `[rwi]` tokens configured
  * `OPENAI_API_KEY` set in the environment (the PBX resolves the realtime
    credential via the env fallback — no [[realtime]] config required)
  * a registered softphone (or sipbot) to answer the originated call

What it does:
  1. originates a call to a destination of your choice via RWI
  2. on answer, starts the `realtime` app pointed at OpenAI's realtime
     endpoint (`wss://api.openai.com/v1/realtime?model=...`)
  3. streams RWI events to stdout — you should see `realtime_event`s
     (connected / transcripts) as you speak, and the AI replies as audio

Usage:
  OPENAI_API_KEY=sk-... python3 examples/realtime_openai_smoke.py \
      --dest 1002 --pbx ws://127.0.0.1:8088/rwi/v1?token=RWI_TOKEN
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys

import aiohttp


async def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--pbx", required=True, help="RWI WebSocket URL incl. token")
    ap.add_argument("--dest", default="1002", help="destination to dial (registered UA)")
    ap.add_argument(
        "--model",
        default="gpt-4o-realtime-preview",
        help="OpenAI realtime model id",
    )
    ap.add_argument("--instructions", default="You are a friendly phone assistant. Keep replies short.")
    ap.add_argument("--seconds", type=float, default=30.0, help="how long to stay on the call")
    args = ap.parse_args()

    if not os.environ.get("OPENAI_API_KEY"):
        print("error: OPENAI_API_KEY is not set — the realtime app uses it as the env fallback", file=sys.stderr)
        return 2

    session = aiohttp.ClientSession()
    ws = await session.ws_connect(args.pbx)
    call_id = f"rt-smoke-{os.getpid()}"

    async def send(action: str, params: dict) -> None:
        await ws.send_str(json.dumps({"rwi": "1.0", "action_id": action, "action": action, "params": params}))

    await send("session.subscribe", {"contexts": ["default"]})
    await send(
        "call.originate",
        {
            "call_id": call_id,
            "destination": args.dest,
            "caller_id": "sip:realtime-smoke@pbx",
            "context": "default",
            "timeout_secs": 30,
        },
    )
    print(f"[smoke] originating {call_id} → {args.dest}; say something when it answers…")

    deadline = asyncio.get_event_loop().time() + args.seconds
    app_started = False
    try:
        async for msg in ws:
            if msg.type != aiohttp.WSMsgType.TEXT:
                continue
            v = json.loads(msg.data)
            et = v.get("event_type") or v.get("type")
            payload = v.get("data") or v
            if et == "call_answered" and not app_started:
                app_started = True
                await send(
                    "call.app_start",
                    {
                        "call_id": call_id,
                        "app_name": "realtime",
                        "params": {
                            "url": "wss://api.openai.com/v1/realtime",
                            "protocol": "openai",
                            "model": args.model,
                            "instructions": args.instructions,
                            # OPENAI_API_KEY is picked up by the env fallback.
                        },
                    },
                )
                print("[smoke] answered — realtime app started")
            elif et == "realtime_event":
                kind = payload.get("kind")
                data = payload.get("data") or {}
                if kind == "transcript_final":
                    print(f"[caller ] {data.get('text', '')}")
                elif kind == "transcript_delta":
                    print(f"[assistant… ] {data.get('text', '')}", end="\r")
                elif kind == "barge_in":
                    print("\n[smoke] barge-in — playback muted")
                elif kind == "connected":
                    print("[smoke] realtime WS connected")
            elif et in ("call_hangup", "call_failed"):
                print(f"[smoke] call ended: {et}")
                break
            if asyncio.get_event_loop().time() > deadline:
                print("\n[smoke] time is up")
                break
    finally:
        await send("call.hangup", {"call_id": call_id})
        await ws.close()
        await session.close()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
