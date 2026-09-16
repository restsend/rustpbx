#!/usr/bin/env python3
"""
generate_failure_sounds.py

Generate Chinese call-failure voice prompts (busy / offline / not-found /
no-answer / service-unavailable) using Microsoft Edge TTS, written to
`config/sounds/` as `<key>-zh.wav`.

These back the `[proxy.audio_profile]` failure-tone slots, e.g.:

    [proxy.audio_profile]
    busy     = "sounds/failure-busy-zh.wav"
    offline  = "sounds/failure-offline-zh.wav"
    notfound = "sounds/failure-notfound-zh.wav"
    noanswer = "sounds/failure-noanswer-zh.wav"
    error    = "sounds/failure-service-zh.wav"

Output format: 16 kHz mono 16-bit PCM WAV (matches the shipped
`config/sounds/queue-*-zh.wav` prompts).

Usage:
    python3 scripts/generate_failure_sounds.py [key ...]

Requirements:
    pip install edge-tts
    brew install ffmpeg  (or apt-get install ffmpeg)
"""

import asyncio
import subprocess
import sys
from pathlib import Path

VOICE = "zh-CN-XiaoxiaoNeural"  # same warm voice as the voicemail prompts
SAMPLE_RATE = 16000  # matches config/sounds/queue-*-zh.wav

# key -> (output file stem, prompt text)
PROMPTS = {
    "busy": (
        "failure-busy-zh",
        "您拨打的电话正在通话中，请稍后再拨。",
    ),
    "offline": (
        "failure-offline-zh",
        "您拨打的电话暂时无法接通，请稍后再拨。",
    ),
    "notfound": (
        "failure-notfound-zh",
        "您拨打的号码是空号，请查证后再拨。",
    ),
    "noanswer": (
        "failure-noanswer-zh",
        "您拨打的电话无人接听，请稍后再拨。",
    ),
    "service": (
        "failure-service-zh",
        "对不起，您拨打的电话暂时不能使用，请稍后再拨。",
    ),
}

OUTPUT_DIR = Path(__file__).resolve().parent.parent / "config" / "sounds"


async def generate_tts(text: str, mp3_path: str):
    """Call edge-tts to produce an MP3 file."""
    import edge_tts

    communicate = edge_tts.Communicate(text, VOICE)
    await communicate.save(mp3_path)


def mp3_to_wav(mp3_path: str, wav_path: str):
    """Convert MP3 to telephony-grade WAV (16kHz mono 16-bit PCM, loudnorm)."""
    subprocess.run(
        [
            "ffmpeg", "-y", "-i", mp3_path,
            "-ar", str(SAMPLE_RATE), "-ac", "1", "-sample_fmt", "s16",
            "-af", "loudnorm=I=-16:TP=-1.5:LRA=11",
            wav_path,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=True,
    )


async def generate(key: str):
    stem, text = PROMPTS[key]
    wav_file = OUTPUT_DIR / f"{stem}.wav"
    if wav_file.exists():
        print(f"  [skip] {wav_file.name} (exists; delete it to regenerate)")
        return

    mp3_file = OUTPUT_DIR / f"{stem}.mp3"
    print(f"  [tts]  {stem}.wav")
    await generate_tts(text, str(mp3_file))
    mp3_to_wav(str(mp3_file), str(wav_file))
    mp3_file.unlink(missing_ok=True)


async def main():
    keys = sys.argv[1:] if len(sys.argv) > 1 else list(PROMPTS.keys())
    for key in keys:
        if key not in PROMPTS:
            print(f"Unknown prompt: {key}. Available: {', '.join(PROMPTS)}")
            sys.exit(1)

    if subprocess.run(["which", "ffmpeg"], capture_output=True).returncode != 0:
        print("ERROR: ffmpeg not found. Install with: brew install ffmpeg")
        sys.exit(1)

    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    print(f"Generating Chinese failure prompts (voice: {VOICE}) -> {OUTPUT_DIR}")
    for key in keys:
        await generate(key)
    print("All done!")


if __name__ == "__main__":
    asyncio.run(main())
