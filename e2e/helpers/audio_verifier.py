"""Audio content verification helpers for E2E tests.

Ported from the removed Rust `tests/helpers/audio_verifier.rs` so Python tests
can assert recorded audio *content* (sine generation, RMS energy, dominant
frequency via FFT-style search, Goertzel magnitude) — not just RTP packet counts.
"""

from __future__ import annotations

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
