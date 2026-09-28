"""Audio content verification helpers for E2E tests.

Ported from the removed Rust `tests/helpers/audio_verifier.rs` so Python tests
can assert recorded audio *content* (sine generation, RMS energy, dominant
frequency via FFT-style search, Goertzel magnitude) — not just RTP packet counts.
"""

from __future__ import annotations

import asyncio
import math
import struct
import wave
from pathlib import Path
from typing import Optional, Tuple

import numpy as np


# ---------------------------------------------------------------------------
# WAV generation
# ---------------------------------------------------------------------------

def generate_sine_wav(
    path: str | Path,
    freq_hz: float,
    duration_s: float,
    sample_rate: int = 8000,
    amplitude: float = 0.5,
) -> Path:
    """Write a 16-bit PCM mono WAV containing a sine wave."""
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    n = int(sample_rate * duration_s)
    samples = (
        amplitude * 32767.0 * np.sin(2 * np.pi * freq_hz * np.arange(n) / sample_rate)
    )
    data = samples.astype(np.int16).tobytes()
    with wave.open(str(p), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(sample_rate)
        w.writeframes(data)
    return p


def create_wav_file(path: str | Path, sample_count: int, sample_rate: int = 8000) -> Path:
    """Write a 440 Hz sine WAV with amplitude 16000 (matches Rust test fixture)."""
    return generate_sine_wav(path, 440.0, sample_count / sample_rate, sample_rate, 16000.0 / 32767.0)


# ---------------------------------------------------------------------------
# WAV reading
# ---------------------------------------------------------------------------

def _read_wav(path: str | Path) -> Tuple[np.ndarray, int]:
    """Parse a RIFF/WAVE file manually — robust to sipbot's RIFF `size=0` header
    and extra chunks that Python's stdlib `wave` rejects.

    Supports linear PCM (format tag 1) and G.711 mu-law (format tag 7), which
    is what the sipflow WAV exporter emits for PCMU calls.
    """
    data = Path(path).read_bytes()
    if len(data) < 12 or data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError("not a WAVE file")
    n_channels, sample_rate, sampwidth, audio_format = 1, 8000, 2, 1
    pcm: bytes = b""
    off = 12
    while off + 8 <= len(data):
        cid = data[off : off + 4]
        size = int.from_bytes(data[off + 4 : off + 8], "little")
        body = data[off + 8 : off + 8 + size]
        if cid == b"fmt ":
            if size >= 14:
                audio_format = int.from_bytes(body[0:2], "little")
                n_channels = int.from_bytes(body[2:4], "little")
                sample_rate = int.from_bytes(body[4:8], "little")
                bits = int.from_bytes(body[14:16], "little")
                sampwidth = bits // 8
        elif cid == b"data":
            # sipbot writes `data` size = 0 and streams PCM to EOF; treat the
            # remainder as the payload when the declared size is 0.
            if size == 0:
                pcm = data[off + 8 :]
            else:
                pcm = body
        off += 8 + size + (size & 1)  # chunks are word-aligned
    if not pcm:
        raise ValueError("no data chunk in WAV")
    if audio_format == 1:  # linear PCM
        if sampwidth != 2:
            raise ValueError(f"expected 16-bit PCM, got {sampwidth * 8}-bit")
        count = len(pcm) // 2 // n_channels
        samples = np.frombuffer(pcm, dtype="<i2").astype(np.int16).reshape(count, n_channels)
    elif audio_format == 7:  # G.711 mu-law
        linear = np.array([_ulaw2linear(b) for b in pcm], dtype=np.int16)
        count = len(linear) // n_channels
        samples = linear[: count * n_channels].reshape(count, n_channels)
    else:
        raise ValueError(f"unsupported WAV format tag {audio_format}")
    return samples, sample_rate


def _ulaw2linear(ulawbyte: int) -> int:
    """Decode one 8-bit mu-law sample to a 16-bit linear PCM sample."""
    u = ~ulawbyte & 0xFF
    sign = u & 0x80
    exponent = (u >> 4) & 0x07
    mantissa = u & 0x0F
    sample = ((mantissa << 3) + 0x84) << exponent
    sample -= 0x84
    return (-sample if sign else sample)


def read_wav_stereo(path: str | Path) -> Tuple[np.ndarray, np.ndarray, int]:
    """Return (rx_ch, tx_ch, sample_rate). Stereo: ch0=RX, ch1=TX (rustpbx convention)."""
    samples, rate = _read_wav(path)
    if samples.shape[1] == 1:
        return samples[:, 0], np.zeros(samples.shape[0], dtype=np.int16), rate
    return samples[:, 0], samples[:, 1], rate


def read_wav_mono(path: str | Path) -> Tuple[np.ndarray, int]:
    samples, rate = _read_wav(path)
    if samples.shape[1] > 1:
        samples = samples.mean(axis=1)
    return samples, rate


# ---------------------------------------------------------------------------
# Signal analysis (mirrors tests/helpers/audio_verifier.rs)
# ---------------------------------------------------------------------------

def find_signal_start(samples: np.ndarray, threshold: float = 0.01, frame: int = 160) -> int:
    """Index of first frame whose peak energy exceeds *threshold*."""
    peak = np.max(np.abs(samples)) if samples.size else 0
    if peak <= 0:
        return 0
    norm = samples.astype(np.float32) / max(peak, 1)
    step = max(frame, 1)
    for i in range(0, max(len(norm) - step, 0), step):
        frame_peak = np.max(np.abs(norm[i : i + step]))
        if frame_peak > threshold:
            return i
    return 0


def extract_audio_region(
    samples: np.ndarray,
    sample_rate: int,
    start: int,
    n_samples: int = 1000,
) -> np.ndarray:
    end = min(start + n_samples, len(samples))
    return samples[start:end]


def compute_rms_db(samples: np.ndarray) -> float:
    """RMS of samples expressed in dBFS (20*log10)."""
    if samples.size == 0:
        return float("-inf")
    rms = math.sqrt(float(np.mean(np.square(samples.astype(np.float64)))))
    if rms <= 0:
        return float("-inf")
    return 20.0 * math.log10(rms / 32767.0)


def has_audio_content(samples: np.ndarray, threshold_db: float) -> bool:
    return compute_rms_db(samples) > threshold_db


def _hann(i: int, n: int) -> float:
    return 0.5 * (1.0 - math.cos(2.0 * math.pi * i / max(n - 1, 1)))


def find_dominant_frequency(
    samples: np.ndarray,
    sample_rate: int,
    low: float = 200.0,
    high: float = 800.0,
    step: float = 5.0,
) -> Tuple[float, float]:
    """Brute-force power estimate; return (best_freq_hz, best_magnitude)."""
    samples = np.asarray(samples).ravel()
    n = len(samples)
    if n == 0:
        return 0.0, 0.0
    win = np.array([_hann(i, n) for i in range(n)])
    sig = samples.astype(np.float64) * win
    best_freq, best_mag = 0.0, -1.0
    freqs = np.arange(low, high + step, step)
    t = np.arange(n) / sample_rate
    for f in freqs:
        re = float(np.sum(sig * np.cos(2 * np.pi * f * t)))
        im = float(np.sum(sig * np.sin(2 * np.pi * f * t)))
        mag = math.hypot(re, im)
        if mag > best_mag:
            best_freq, best_mag = f, mag
    return best_freq, best_mag


def goertzel_magnitude_normalized(samples: np.ndarray, target_freq: float, sample_rate: int) -> float:
    """Normalized Goertzel magnitude at *target_freq* (0..1 scale)."""
    samples = np.asarray(samples).ravel()
    n = len(samples)
    if n == 0:
        return 0.0
    k = n * target_freq / sample_rate
    w = 2.0 * math.pi * k / n
    coeff = 2.0 * math.cos(w)
    s_prev, s_prev2 = 0.0, 0.0
    for s in samples.astype(np.float64):
        s_cur = s + coeff * s_prev - s_prev2
        s_prev2, s_prev = s_prev, s_cur
    power = s_prev2 * s_prev2 + s_prev * s_prev - coeff * s_prev * s_prev2
    return math.sqrt(max(power, 0.0)) / n


# ---------------------------------------------------------------------------
# Shared assertion layer (2026-09 audio-assertion audit).
#
# Three dimensions every call test must gate on:
#   content  — the expected tone/spectrum is actually audible (window RMS +
#              dominant frequency / Goertzel), not just packets;
#   format   — the negotiated/bridged codec is what the scenario intends
#              (PBX transcode log, UA-reported codec, or WAV header);
#   quantity — sane rates/durations (pps ≈ 50±20% for 20 ms packing,
#              audio-band byte rate ceiling catches un-negotiated video
#              leak, recording length ≈ call window).
# ---------------------------------------------------------------------------


def band_peak_db(samples: np.ndarray, sample_rate: int, freq: float, width: float = 15.0) -> float:
    """Peak magnitude (dB) of *samples* within freq±width via rFFT."""
    samples = np.asarray(samples, dtype=float)
    n = samples.size
    if n < 16:
        return float("-inf")
    win = np.hanning(n)
    spec = np.abs(np.fft.rfft(samples * win))
    freqs = np.fft.rfftfreq(n, 1 / sample_rate)
    mask = (freqs >= freq - width) & (freqs <= freq + width)
    if not mask.any():
        return float("-inf")
    return float(20 * np.log10(spec[mask].max() + 1e-9))


def window_rms_db(samples: np.ndarray, sample_rate: int, t0: float, t1: float):
    """RMS dBFS + dominant frequency (Hz) of samples[t0*sr : t1*sr];
    (None, None) when the window is too short."""
    seg = samples[int(t0 * sample_rate):int(t1 * sample_rate)].astype(float)
    if seg.size < sample_rate // 4:
        return None, None
    rms = compute_rms_db(seg)
    best_freq, _mag = find_dominant_frequency(seg, sample_rate)
    return rms, best_freq


def goertzel_timeline(
    samples: np.ndarray,
    sample_rate: int,
    freq: float,
    window_s: float = 0.25,
    step_s: float = 0.25,
) -> list:
    """Goertzel magnitude at *freq* over sliding windows of the recording."""
    samples = np.asarray(samples).ravel()
    win = int(sample_rate * window_s)
    step = int(sample_rate * step_s)
    out = []
    for off in range(0, max(samples.size - win + 1, 0), max(step, 1)):
        out.append(goertzel_magnitude_normalized(samples[off:off + win], freq, sample_rate))
    return out


def wait_recording(record_path, timeout: float = 40.0):
    """Poll for a sipbot mixdown recording (SYNC — blocking; prefer the
    async twin :func:`wait_recording_async` inside async tests). sipbot
    suffixed the filename and streams PCM as the call progresses, so a
    plain ``exists()`` check on the requested path misses it — glob
    ``stem*.wav`` instead (same pattern test_g729 / test_flow1 use).
    Returns the resolved Path."""
    import time

    p = Path(record_path)
    deadline = time.monotonic() + timeout
    while True:
        hits = sorted(p.parent.glob(p.stem + "*.wav"))
        if hits:
            return hits[-1]
        if time.monotonic() >= deadline:
            return None
        time.sleep(0.5)


async def wait_recording_async(record_path, timeout: float = 40.0):
    """Async twin of :func:`wait_recording` — awaits between polls so the
    event loop (webhook receiver / RWI client) keeps running. A blocking
    ``time.sleep`` inside an async test starves those receivers and can
    turn into lost events / false failures."""
    p = Path(record_path)
    loop = asyncio.get_event_loop()
    deadline = loop.time() + timeout
    while True:
        hits = sorted(p.parent.glob(p.stem + "*.wav"))
        if hits:
            return hits[-1]
        if loop.time() >= deadline:
            return None
        await asyncio.sleep(0.5)


def assert_tone_window(
    samples: np.ndarray,
    sample_rate: int,
    t0: float,
    t1: float,
    freq: float,
    *,
    floor_db: float = -40.0,
    tol_hz: float = 15.0,
    label: str = "window",
):
    """CONTENT gate: samples[t0:t1] carries *freq* above the silence floor."""
    rms, dom = window_rms_db(samples, sample_rate, t0, t1)
    assert rms is not None, f"{label}: window too short to analyse"
    assert rms >= floor_db, f"{label}: window silent (rms={rms:.1f}dB < {floor_db})"
    assert dom is not None and abs(dom - freq) <= tol_hz, (
        f"{label}: dominant {dom:.0f}Hz, expected {freq:.0f}Hz(±{tol_hz}) "
        f"(rms={rms:.1f}dB)"
    )
    return rms, dom


def assert_play_then_restore(
    samples: np.ndarray,
    sample_rate: int,
    *,
    prompt_freq: float,
    conv_freq: float,
    play_t0: float,
    play_t1: float,
    restore_t0: float,
    restore_t1: float,
    base_t0: float = 0.5,
    base_t1: float = 2.5,
    label: str = "play",
):
    """★ media.play core gate — three-phase Goertzel timeline, both statements:
      ① 播放段: prompt_freq energy ≥ 10× conversation baseline (audible);
      ② 播放结束: prompt energy falls back to baseline (play really stopped);
      ③ 恢复段: conversation tone returns (能继续听得到) — content, not packets.
    """
    base_med, play_peak, restore_mag = _phase_magnitudes(
        samples, sample_rate, prompt_freq, conv_freq,
        base_t0, base_t1, play_t0, play_t1, restore_t0, restore_t1,
    )
    assert play_peak >= max(base_med * 10.0, 500.0), (
        f"{label}: prompt NOT audible — play-window Goertzel peak "
        f"{play_peak:.1f} vs conversation baseline median {base_med:.1f}"
    )
    assert restore_mag <= max(base_med * 3.0, 200.0), (
        f"{label}: prompt still ringing after it should have ended — "
        f"restore-window prompt magnitude {restore_mag:.1f} vs baseline "
        f"{base_med:.1f}"
    )
    rms, dom = window_rms_db(samples, sample_rate, restore_t0, restore_t1)
    assert rms is not None and rms >= -40.0, (
        f"{label}: restore window silent — the call did not resume audibly"
    )
    assert dom is not None and abs(dom - conv_freq) <= 15.0, (
        f"{label}: restore window dominant {dom:.0f}Hz, want conversation "
        f"{conv_freq:.0f}Hz — media route restored but the peer audio did "
        "not come back"
    )
    return base_med, play_peak, restore_mag


def _phase_magnitudes(
    samples, sample_rate, prompt_freq, conv_freq,
    base_t0, base_t1, play_t0, play_t1, restore_t0, restore_t1,
):
    """Goertzel magnitudes at prompt_freq across the three phases, plus the
    conversation-band baseline median (10× rule reference). *samples* is the
    FULL recording; phase coordinates are absolute seconds."""
    def mag(t0, t1):
        seg = samples[int(t0 * sample_rate):int(t1 * sample_rate)].astype(float)
        if seg.size < sample_rate // 8:
            return 0.0
        return goertzel_magnitude_normalized(seg, prompt_freq, sample_rate)

    base_timeline = goertzel_timeline(
        samples, sample_rate, prompt_freq, window_s=0.25, step_s=0.125,
    )
    finite = [m for m in base_timeline if m > 0]
    base_med = sorted(finite)[len(finite) // 2] if finite else 0.0
    return base_med, mag(play_t0, play_t1), mag(restore_t0, restore_t1)


def assert_server_recording(
    path,
    *,
    tone_hz: float | None = None,
    min_s: float,
    max_s: float | None = None,
    expect_rate: int | None = None,
    label: str = "server recording",
):
    """3D gate for a SERVER-side recording (config/recorders/*.wav):
      format  — RIFF/WAVE parse via the robust reader (sample rate / channels);
      quantity— duration within [min_s, max_s];
      content — dominant tone when *tone_hz* given.
    Returns (samples_mono, sample_rate)."""
    hits = sorted(Path(path).parent.glob(Path(path).stem + "*.wav")) or \
        ([Path(path)] if Path(path).exists() else [])
    assert hits, f"{label}: recording missing at {path}"
    resolved = hits[-1]
    samples, rate = read_wav_mono(resolved)
    duration = samples.size / rate
    assert duration >= min_s, (
        f"{label}: {duration:.1f}s recording shorter than the {min_s}s window "
        f"(rate={rate}) — audio was not captured for the whole call"
    )
    if max_s is not None:
        assert duration <= max_s, (
            f"{label}: {duration:.1f}s recording exceeds the {max_s}s window"
        )
    if expect_rate is not None:
        assert rate == expect_rate, (
            f"{label}: sample rate {rate} != expected {expect_rate} — wrong "
            "decode path for this codec"
        )
    if tone_hz is not None:
        start = find_signal_start(samples)
        region = samples[start:min(start + 5 * rate, samples.size)]
        rms = compute_rms_db(region)
        assert rms >= -40.0, f"{label}: recording too quiet ({rms:.1f}dB)"
        dom, _mag = find_dominant_frequency(region, rate, low=200, high=900, step=5)
        assert abs(dom - tone_hz) <= 15.0, (
            f"{label}: dominant {dom:.0f}Hz, want {tone_hz:.0f}Hz — content "
            "corrupted or one-way"
        )
    return samples, rate


def negotiated_codec(ua) -> str:
    """Last 'codec: X' line from a sipbot UA's output (SDP answer result)."""
    import re
    matches = re.findall(r"(?:codec|Codec):\s*([A-Za-z0-9/]+)", ua.output)
    return matches[-1] if matches else ""


def longest_above_run(timeline, threshold: float, step_s: float):
    """Longest contiguous run above *threshold* in a Goertzel timeline.
    Returns (start_idx, end_idx_exclusive, duration_s). Robust to phase
    offsets — locate the play window by content, not by wall clock."""
    best = (0, 0, 0.0)
    cur_start = None
    for i, m in enumerate(timeline):
        if m >= threshold:
            if cur_start is None:
                cur_start = i
        else:
            if cur_start is not None:
                dur = (i - cur_start) * step_s
                if dur > best[2]:
                    best = (cur_start, i, dur)
                cur_start = None
    if cur_start is not None:
        dur = (len(timeline) - cur_start) * step_s
        if dur > best[2]:
            best = (cur_start, len(timeline), dur)
    return best


def assert_media_quantity(stats, elapsed_s: float, *, audio_max_bps: int = 12000) -> None:
    """QUANTITY gate: packet rate sane for 20 ms packing (~50 pps) and the
    RX byte rate stays in audio territory (an un-negotiated video track
    floods ~100 kbit/s; opus+RTP overhead ≈ 6.6 kB/s)."""
    if elapsed_s <= 0:
        return
    pps = stats.rx_packets / elapsed_s
    assert pps >= 20.0, (
        f"RX packet rate {pps:.0f}/s over {elapsed_s:.0f}s is below the "
        "20 ms-packing floor (~50/s) — media starving"
    )
    bps = stats.rx_bytes / elapsed_s
    assert bps < audio_max_bps, (
        f"RX byte rate {bps:.0f}B/s exceeds the audio ceiling {audio_max_bps} "
        "— an un-negotiated media stream (video?) is leaking into the leg"
    )
