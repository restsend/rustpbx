#!/usr/bin/env python3
"""Generate the built-in music assets shipped with the PBX (8 kHz mono PCM16 —
the native telephone rate, so the hold gate / queue player never resamples).

    python3 tools/gen_builtin_music.py

Outputs (both seamless loops, original & royalty-free — no third-party
composition, so they can be bundled without licensing obligations):

* ``config/sounds/queue-waiting-8k.wav`` — caller **queued / no agent
  available** (`[proxy] queue_hold_music` overrides).
* ``config/sounds/hold-music-8k.wav`` — music heard by the party **on hold**
  (`[proxy] hold_music` overrides).

Style: Chinese guzheng (古筝) pentatonic (五声) over soft major harmony —
rolled chords and broken-chord accompaniment, a lead line (single plucks with
按音 bend, 摇指 tremolo on long notes) with grace notes (倚音), plus a soft
string pad for warmth. No flute (笛) on purpose — a high bamboo flute is
tiring to hear on a long queue/hold loop. Major pentatonic on purpose: the
earlier minor-pentatonic/portamento version read as "eerie"; these are meant
to feel comfortable and bright (舒适、明亮) while staying unmistakably Chinese.

The two loops are deliberately in **different keys** so a caller can tell
"still queued" from "on hold" by ear alone:

* queue waiting — **A 宫** (A B C# E F#), 92 BPM, forward moving with a
  quarter-note bass pulse; the lead deliberately sits in the warm mid
  register (**E4–C#5**, ~330–554 Hz) with no high-register line — a caller
  can sit on this loop for minutes and high notes wear out fast.
* hold music — **D 宫** (D E F# A B), 84 BPM, slower and airier.

They previously shared key, tempo and melodic contour, which made the two
half-identifiable with each other over the phone.

Both are deliberately different from the agent ringback
(``sounds/phone-calling.wav``), which is only played while an agent's leg is
actually ringing.

Rendering is deterministic (fixed seeds), so re-running reproduces the assets
byte-for-byte.
"""
import math
import sys
import wave
from pathlib import Path

import numpy as np

SR = 8000
ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / "config" / "sounds"

_PITCH = {"C": 0, "C#": 1, "D": 2, "D#": 3, "E": 4, "F": 5,
          "F#": 6, "G": 7, "G#": 8, "A": 9, "A#": 10, "B": 11}


def down(note: str, octaves: int = 1) -> str:
    """Transpose a note name down by whole octaves (keeps the same spelling)."""
    return f"{note[:-1]}{int(note[-1]) - octaves}"


def hz(name: str) -> float:
    """Scientific pitch: 'A4' -> 440.0."""
    pitch, octave = name[:-1], int(name[-1])
    semis = _PITCH[pitch] + (octave - 4) * 12
    return 440.0 * (2.0 ** ((semis - 9) / 12.0))


# ── instruments ────────────────────────────────────────────────────────
def guzheng(buf: np.ndarray, start: float, note: str, dur: float, amp: float,
            rng: np.random.Generator, bright: float = 1.0,
            bend: float = 0.0) -> None:
    """Plucked zither voice: bright attack, per-partial decay, optional 揉弦."""
    f0 = hz(note)
    i0 = int(start * SR)
    if i0 < 0:
        # pickup (倚音) scheduled slightly before t=0: truncate the pre-roll
        # instead of writing outside the buffer.
        n = min(int(dur * SR) + i0, len(buf))
        if n <= 0:
            return
        i0 = 0
    else:
        n = min(int(dur * SR), len(buf) - i0)
        if n <= 0:
            return
    t = np.arange(n) / SR
    freq = f0 * (1.0 + bend * np.exp(-t / 0.15))
    phase = 2.0 * math.pi * np.cumsum(freq) / SR
    sig = np.zeros(n)
    for k in range(1, 8):
        level = bright / (k ** 1.30) * np.exp(-(1.20 + 0.50 * k) * t)
        sig += level * np.sin(k * phase)
    sig += rng.standard_normal(n) * np.exp(-170.0 * t) * 0.04   # 指甲触弦
    atk = int(0.003 * SR)
    if atk > 1:
        sig[:atk] *= np.linspace(0.0, 1.0, atk)
    rel = min(n, int(0.06 * SR))
    if rel > 1:
        sig[n - rel:] *= np.linspace(1.0, 0.0, rel)
    buf[i0:i0 + n] += amp * sig


def rolled_chord(buf: np.ndarray, start: float, notes: list[str], step: float,
                 amp: float, rng: np.random.Generator) -> None:
    """琶音/刮奏-style roll: quick low→high sweep on the chord tones."""
    for i, note in enumerate(notes):
        guzheng(buf, start + i * step, note, 2.4, amp * (0.85 + 0.05 * i), rng,
                bright=0.9)


def lead(buf: np.ndarray, start: float, note: str, dur: float, amp: float,
         rng: np.random.Generator) -> None:
    """Guzheng lead voice (no flute — a high dizi is tiring on a long loop).

    Short notes are single plucks with a touch of 按音 (press-bend); long notes
    use 摇指 (tremolo), the guzheng technique for sustaining a melody line.
    """
    if dur >= 1.15:
        # 摇指: initial accent, then rapid light plucks with a soft swell
        guzheng(buf, start, note, 0.9, amp * 1.15, rng, bright=0.75)
        step = 0.065
        k = 0
        t = step
        while t + step < dur:
            swell = math.sin(math.pi * (t / dur)) ** 0.6
            guzheng(buf, start + t, note, 0.42, amp * 0.55 * max(0.25, swell),
                    rng, bright=0.6)
            t += step
            k += 1
    else:
        guzheng(buf, start, note, max(1.1, dur * 2.2), amp * 1.2, rng,
                bright=0.8, bend=-0.004)


def warm_pad(buf: np.ndarray, start: float, notes: list[str], dur: float,
             amp: float) -> None:
    """Soft string-pad chord: detuned ensemble, slow attack, gentle release.

    This is what makes the loop feel comfortable instead of thin/hollow.
    """
    n = min(int(dur * SR), len(buf) - int(start * SR))
    if n <= 0:
        return
    i0 = int(start * SR)
    t = np.arange(n) / SR
    env = np.minimum(1.0, t / 0.45) * np.minimum(1.0, (dur - t) / 1.10).clip(0.0, 1.0)
    sig = np.zeros(n)
    for note in notes:
        base = hz(note)
        for detune, weight in ((1.0, 0.5), (1.0009, 0.25), (0.9991, 0.25)):
            f = base * detune
            sig += weight * (np.sin(2.0 * math.pi * f * t)
                             + 0.30 * np.sin(2.0 * math.pi * 2.0 * f * t))
    sig /= max(1.0, len(notes))
    buf[i0:i0 + n] += amp * env * sig


def lowpass(x: np.ndarray, cutoff: float) -> np.ndarray:
    alpha = 1.0 - math.exp(-2.0 * math.pi * cutoff / SR)
    y = np.empty_like(x)
    acc = 0.0
    for i in range(len(x)):
        acc += alpha * (x[i] - acc)
        y[i] = acc
    return y


def reverb(buf: np.ndarray, taps) -> np.ndarray:
    """Short, bright room (a long hall reads as spooky)."""
    out = buf.copy()
    for delay, gain in taps:
        d = int(delay * SR)
        if d < len(buf):
            out[d:] += buf[:-d] * gain
    return out


def write_wav(path: Path, signal: np.ndarray, peak: float) -> None:
    hi = float(np.max(np.abs(signal))) or 1.0
    out = np.clip(signal * (peak / hi), -1.0, 1.0)
    pcm = (out * 32767.0).astype("<i2")
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(pcm.tobytes())
    rms = float(np.sqrt(np.mean(pcm.astype(np.float64) ** 2)))
    print("wrote %-52s %6.2fs  %8d B  peak=%.2f  rms=%.0f" % (
        str(path.relative_to(ROOT)), len(pcm) / SR, len(pcm) * 2, peak, rms))


# ── 1. queue waiting music (A 宫, 92 BPM, 8 bars) ──────────────────────
QUEUE_BPM = 92.0
QUEUE_BEAT = 60.0 / QUEUE_BPM
QUEUE_BAR = 4 * QUEUE_BEAT
# I–V–vi–IV in A major, ending on V so the loop seam resolves into bar 1.
QUEUE_CHORDS = [
    ("A2", ["A3", "C#4", "E4"], ["A3", "C#4", "E4", "A4"]),
    ("E2", ["E3", "G#3", "B3"], ["E3", "G#3", "B3", "E4"]),
    ("F#2", ["F#3", "A3", "C#4"], ["F#3", "A3", "C#4", "F#4"]),
    ("D2", ["D3", "F#3", "A3"], ["D3", "F#3", "A3", "D4"]),
    ("A2", ["A3", "C#4", "E4"], ["A3", "C#4", "E4", "A4"]),
    ("E2", ["E3", "G#3", "B3"], ["E3", "G#3", "B3", "E4"]),
    ("D2", ["D3", "F#3", "A3"], ["D3", "F#3", "A3", "D4"]),
    ("E2", ["E3", "G#3", "B3"], ["E3", "G#3", "B3", "E4"]),
]
# Written an octave above the sounding pitch (lead() applies down()), so the
# lead actually sounds between E4 (330 Hz) and C#5 (554 Hz) — a warm
# mid-range line. Deliberately low: a caller may sit on this loop for
# minutes, and a bright high register gets tiring fast. A 宫 pentatonic
# only: A B C# E F#.
QUEUE_MELODY = [
    (0, 0.0, "E5", 1.0, "F#5", 0.30), (0, 1.0, "A5", 1.0, None, 0.29),
    (0, 2.0, "C#6", 2.0, None, 0.30),
    (1, 0.0, "C#6", 1.0, None, 0.28), (1, 1.0, "B5", 1.0, None, 0.28),
    (1, 2.0, "A5", 1.0, None, 0.28), (1, 3.0, "F#5", 1.0, None, 0.27),
    (2, 0.0, "A5", 2.0, None, 0.29), (2, 2.0, "F#5", 1.0, None, 0.27),
    (2, 3.0, "A5", 1.0, None, 0.28),
    (3, 0.0, "B5", 1.5, None, 0.28), (3, 1.5, "A5", 0.5, None, 0.27),
    (3, 2.0, "F#5", 2.0, None, 0.28),
    (4, 0.0, "E5", 2.0, None, 0.29), (4, 2.0, "A5", 1.0, None, 0.28),
    (4, 3.0, "C#6", 1.0, None, 0.28),
    (5, 0.0, "C#6", 1.5, None, 0.28), (5, 1.5, "B5", 0.5, None, 0.27),
    (5, 2.0, "A5", 2.0, None, 0.29),
    (6, 0.0, "F#5", 1.0, None, 0.27), (6, 1.0, "A5", 1.0, None, 0.28),
    (6, 2.0, "B5", 2.0, None, 0.28),
    (7, 0.0, "A5", 1.0, None, 0.27), (7, 1.0, "B5", 1.0, None, 0.28),
    (7, 2.0, "C#6", 1.0, None, 0.28), (7, 3.0, "B5", 1.0, None, 0.28),
]


def gen_queue() -> np.ndarray:
    rng = np.random.default_rng(20261116)
    buf = np.zeros(int((len(QUEUE_CHORDS) * QUEUE_BAR + 1.3) * SR))
    for i, (bass, voices, roll) in enumerate(QUEUE_CHORDS):
        t0 = i * QUEUE_BAR
        # warm pad across the bar, softer than the hold loop so the moving
        # parts (bass pulse / broken chords) stay the foreground
        warm_pad(buf, t0, voices, QUEUE_BAR + 0.8, 0.062)
        # bright arpeggio roll on beat 1, then broken chords in eighths
        rolled_chord(buf, t0, roll, 0.028, 0.145, rng)
        # quarter-note bass pulse — this gentle tread is what reads as
        # "waiting" rather than "drifting" (hold keeps a single drone)
        for beat, amp in ((0.0, 0.135), (1.0, 0.070), (2.0, 0.105), (3.0, 0.070)):
            guzheng(buf, t0 + beat * QUEUE_BEAT, bass, 1.9, amp, rng, bright=0.45)
        pattern = [roll[1], roll[2], roll[3], roll[2],
                   roll[1], roll[3], roll[2], roll[1]]
        for k, note in enumerate(pattern):
            guzheng(buf, t0 + (0.5 + k * 0.5) * QUEUE_BEAT, note, 1.1,
                    0.066, rng, bright=0.60)
    for bar, beat, note, beats, grace, amp in QUEUE_MELODY:
        t0 = bar * QUEUE_BAR + beat * QUEUE_BEAT
        if grace:                                   # 倚音 pickup
            guzheng(buf, t0 - 0.075, down(grace), 0.8, amp * 0.7, rng, bright=0.65)
        lead(buf, t0, down(note), beats * QUEUE_BEAT, amp, rng)
    # shorter room than the hold loop, and rolled off earlier so the loop
    # stays warm instead of fizzy on a telephone earpiece
    taps = ((0.023, 0.18), (0.041, 0.14), (0.061, 0.11), (0.089, 0.08),
            (0.121, 0.05))
    return lowpass(reverb(lowpass(buf, 3400.0), taps), 3800.0)


# ── 2. hold music (D 宫, 68 BPM, 12 bars) ──────────────────────────────
HOLD_BPM = 84.0
HOLD_BEAT = 60.0 / HOLD_BPM
HOLD_BAR = 4 * HOLD_BEAT
HOLD_CHORDS = [
    ("D2", ["D3", "F#3", "A3", "D4"], ["F#4", "A4"]),
    ("B1", ["B2", "D3", "F#3", "B3"], ["D4", "F#4"]),
    ("G2", ["G2", "B2", "D3", "G3"], ["B3", "D4"]),
    ("A2", ["A2", "C#3", "E3", "A3"], ["C#4", "E4"]),
    ("D2", ["D3", "F#3", "A3", "D4"], ["F#4", "A4"]),
    ("B1", ["B2", "D3", "F#3", "B3"], ["D4", "F#4"]),
    ("G2", ["G2", "B2", "D3", "G3"], ["B3", "D4"]),
    ("A2", ["A2", "C#3", "E3", "A3"], ["C#4", "E4"]),
    ("D2", ["D3", "F#3", "A3", "D4"], ["F#4", "A4"]),
    ("G2", ["G2", "B2", "D3", "G3"], ["B3", "D4"]),
    ("A2", ["A2", "C#3", "E3", "A3"], ["C#4", "E4"]),
    ("D2", ["D3", "F#3", "A3", "D4"], ["F#4", "A4"]),
]
# Long, relaxed dizi phrases (pentatonic), lots of air between them.
HOLD_MELODY = [
    (0, 0.0, "F#5", 1.0, "A5", 0.27), (0, 1.0, "E5", 1.0, None, 0.25),
    (0, 2.0, "D5", 2.0, None, 0.27),
    (1, 0.0, "D5", 1.0, None, 0.25), (1, 1.0, "E5", 1.0, None, 0.25),
    (1, 2.0, "F#5", 1.0, None, 0.26), (1, 3.0, "A5", 1.0, None, 0.26),
    (2, 0.0, "B5", 2.0, "A5", 0.27), (2, 2.0, "A5", 1.0, None, 0.25),
    (2, 3.0, "F#5", 1.0, None, 0.25),
    (3, 0.0, "A5", 1.5, None, 0.26), (3, 1.5, "E5", 0.5, None, 0.24),
    (3, 2.0, "D5", 2.0, None, 0.26),
    (4, 0.0, "F#5", 1.0, None, 0.26), (4, 1.0, "A5", 1.0, None, 0.26),
    (4, 2.0, "B5", 2.0, None, 0.26),
    (5, 0.0, "A5", 1.0, None, 0.26), (5, 1.0, "F#5", 1.0, None, 0.25),
    (5, 2.0, "E5", 2.0, None, 0.24),
    (6, 0.0, "D5", 1.0, "F#5", 0.25), (6, 1.0, "E5", 1.0, None, 0.24),
    (6, 2.0, "F#5", 2.0, None, 0.26),
    (7, 0.0, "E5", 1.5, None, 0.25), (7, 1.5, "D5", 0.5, None, 0.23),
    (7, 2.0, "B4", 2.0, None, 0.24),
    (8, 0.0, "F#5", 1.0, "A5", 0.26), (8, 1.0, "A5", 1.0, None, 0.26),
    (8, 2.0, "B5", 2.0, None, 0.26),
    (9, 0.0, "A5", 1.5, None, 0.26), (9, 1.5, "F#5", 0.5, None, 0.24),
    (9, 2.0, "E5", 2.0, None, 0.24),
    (10, 0.0, "D5", 1.0, None, 0.25), (10, 1.0, "E5", 1.0, None, 0.24),
    (10, 2.0, "F#5", 1.0, None, 0.25), (10, 3.0, "E5", 1.0, None, 0.24),
    (11, 0.0, "D5", 4.0, "F#5", 0.28),
]


def gen_hold() -> np.ndarray:
    rng = np.random.default_rng(20261115)
    buf = np.zeros(int((len(HOLD_CHORDS) * HOLD_BAR + 1.4) * SR))
    for i, (bass, voices, sparkle) in enumerate(HOLD_CHORDS):
        t0 = i * HOLD_BAR
        warm_pad(buf, t0, voices, HOLD_BAR + 1.1, 0.082)     # main warmth
        guzheng(buf, t0, bass, 3.6, 0.11, rng, bright=0.42)  # low drone (soft)
        rolled_chord(buf, t0, [voices[1], voices[2], voices[3]], 0.045, 0.13, rng)
        # sparse accompaniment on beats 2 and 4 — light, no metronome feel
        # gentle broken chords on every eighth, plus a light answer on beat 4
        pattern = [sparkle[0], sparkle[1], voices[3], sparkle[0],
                   sparkle[1], voices[2], sparkle[0], sparkle[1]]
        for k, note in enumerate(pattern):
            if k == 0:
                continue          # the rolled chord already covers beat 1
            guzheng(buf, t0 + k * 0.5 * HOLD_BEAT, note, 1.4, 0.070, rng,
                    bright=0.62)
    for bar, beat, note, beats, grace, amp in HOLD_MELODY:
        t0 = bar * HOLD_BAR + beat * HOLD_BEAT
        if grace:
            guzheng(buf, t0 - 0.09, down(grace), 0.9, amp * 0.7, rng, bright=0.7)
        lead(buf, t0, down(note), beats * HOLD_BEAT, amp, rng)
    taps = ((0.041, 0.19), (0.067, 0.15), (0.097, 0.11), (0.137, 0.08),
            (0.191, 0.05))
    return lowpass(reverb(lowpass(buf, 3200.0), taps), 3800.0)


def main() -> int:
    write_wav(OUT_DIR / "queue-waiting-8k.wav", gen_queue(), 0.50)
    write_wav(OUT_DIR / "hold-music-8k.wav", gen_hold(), 0.46)
    return 0


if __name__ == "__main__":
    sys.exit(main())
