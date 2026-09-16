"""Strict audio-content assertions with structured results (evidence-grade).

Wraps helpers.audio_verifier primitives. Every check returns an
:class:`AudioCheckResult` that is JSON-serializable (for evidence.json) and
raises AssertionError on violation. Unreadable / empty / all-silence WAVs are
hard failures — never vacuous passes.
"""

from __future__ import annotations

import math
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional

from .. import audio_verifier as av


@dataclass
class AudioCheckResult:
    """Structured audio measurement — goes into evidence.json verbatim."""

    file: str
    exists: bool = False
    bytes: int = 0
    sample_rate: Optional[int] = None
    channels: Optional[int] = None
    frames: Optional[int] = None
    duration_sec: Optional[float] = None
    rms_db: Optional[float] = None
    dominant_freq_hz: Optional[float] = None
    has_audio_content: Optional[bool] = None
    checks: list = field(default_factory=list)  # [{name, expected, got, pass}]
    ok: bool = False
    error: Optional[str] = None

    def to_dict(self) -> dict:
        return asdict(self)

    def summary(self) -> str:
        parts = [f"{Path(self.file).name}"]
        if self.duration_sec is not None:
            parts.append(f"{self.duration_sec:.2f}s")
        if self.rms_db is not None:
            parts.append(f"{self.rms_db:.1f}dB")
        if self.dominant_freq_hz is not None:
            parts.append(f"{self.dominant_freq_hz:.0f}Hz")
        return " ".join(parts)


def _analyze(path, mono: bool, min_rms_db: float = -45.0):
    p = Path(path)
    res = AudioCheckResult(file=str(p), exists=p.exists(), bytes=p.stat().st_size if p.exists() else 0)
    if not p.exists():
        raise AssertionError(f"[audio] file missing: {p}")
    if p.stat().st_size < 44:
        raise AssertionError(f"[audio] file too small to be WAV: {p} ({res.bytes} bytes)")
    try:
        if mono:
            samples, rate = av.read_wav_mono(str(p))
            res.channels = 1
        else:
            samples, rate, channels = av.read_wav_stereo(str(p))
            res.channels = channels
    except Exception as exc:  # unparsable → hard fail
        raise AssertionError(f"[audio] unreadable WAV {p}: {exc}") from exc
    res.sample_rate = rate
    res.frames = len(samples)
    res.duration_sec = round(len(samples) / rate, 3) if rate else None
    if samples is None or len(samples) == 0:
        raise AssertionError(f"[audio] zero samples in {p}")
    res.rms_db = round(av.compute_rms_db(samples), 2)
    res.has_audio_content = bool(av.has_audio_content(samples, min_rms_db))
    try:
        freq, _mag = av.find_dominant_frequency(samples, rate)
        if freq and not math.isnan(freq):
            res.dominant_freq_hz = round(freq, 1)
    except Exception:
        res.dominant_freq_hz = None
    return res, samples, rate


def _record(res: AudioCheckResult, name: str, expected, got, ok: bool):
    res.checks.append({"name": name, "expected": expected, "got": got, "pass": bool(ok)})
    return ok


def assert_audio(
    wav_path,
    *,
    freq: Optional[float] = None,
    freq_tol_hz: float = 15.0,
    min_rms_db: float = -40.0,
    min_duration: Optional[float] = None,
    max_duration: Optional[float] = None,
    min_channels: Optional[int] = None,
    sample_rate: Optional[int] = None,
    require_content: bool = True,
    label: str = "",
) -> AudioCheckResult:
    """Strictly validate a WAV file's presence, decodability and content.

    Returns AudioCheckResult (attach to evidence); raises AssertionError with
    expected/got on any violation. Silence, emptiness and unreadable files fail.
    """
    res, samples, rate = _analyze(wav_path, mono=True, min_rms_db=min_rms_db)
    prefix = f"[audio]{(' ' + label) if label else ''} {Path(str(wav_path)).name}"

    if require_content:
        ok = bool(res.has_audio_content)
        _record(res, "has_audio_content", True, res.has_audio_content, ok)
        if not ok:
            res.error = "all-silence or near-zero energy"
            raise AssertionError(f"{prefix}: no audio content (rms={res.rms_db}dB) — all-silence invalid")

    ok = res.rms_db is not None and res.rms_db >= min_rms_db
    _record(res, "rms_db", f">= {min_rms_db}", res.rms_db, ok)
    if not ok:
        res.error = f"rms {res.rms_db}dB < floor {min_rms_db}dB"
        raise AssertionError(f"{prefix}: RMS {res.rms_db}dB below floor {min_rms_db}dB")

    if min_duration is not None:
        ok = res.duration_sec is not None and res.duration_sec >= min_duration
        _record(res, "duration_sec", f">= {min_duration}", res.duration_sec, ok)
        if not ok:
            res.error = f"duration {res.duration_sec}s < {min_duration}s"
            raise AssertionError(f"{prefix}: duration {res.duration_sec}s < required {min_duration}s")
    if max_duration is not None:
        ok = res.duration_sec is not None and res.duration_sec <= max_duration
        _record(res, "duration_sec", f"<= {max_duration}", res.duration_sec, ok)
        if not ok:
            res.error = f"duration {res.duration_sec}s > {max_duration}s"
            raise AssertionError(f"{prefix}: duration {res.duration_sec}s exceeded {max_duration}s")

    if min_channels is not None:
        got_ch = res.channels if res.channels is not None else 1
        ok = got_ch >= min_channels
        _record(res, "channels", f">= {min_channels}", got_ch, ok)
        if not ok:
            res.error = f"channels {got_ch} < {min_channels}"
            raise AssertionError(f"{prefix}: {got_ch} channel(s) but required >= {min_channels}")

    if sample_rate is not None:
        ok = rate == sample_rate
        _record(res, "sample_rate", sample_rate, rate, ok)
        if not ok:
            res.error = f"sample_rate {rate} != {sample_rate}"
            raise AssertionError(f"{prefix}: sample rate {rate} != required {sample_rate}")

    if freq is not None and res.dominant_freq_hz is not None:
        delta = abs(res.dominant_freq_hz - freq)
        ok = delta <= freq_tol_hz
        _record(res, "dominant_freq_hz", f"{freq}±{freq_tol_hz}", res.dominant_freq_hz, ok)
        if not ok:
            res.error = f"freq {res.dominant_freq_hz}Hz outside {freq}±{freq_tol_hz}Hz"
            raise AssertionError(
                f"{prefix}: dominant frequency {res.dominant_freq_hz}Hz outside "
                f"{freq}±{freq_tol_hz}Hz (captured duration {res.duration_sec}s, "
                f"rms {res.rms_db}dB)"
            )
    elif freq is not None:
        _record(res, "dominant_freq_hz", f"{freq}±{freq_tol_hz}", None, False)
        res.error = "no dominant frequency detected"
        raise AssertionError(f"{prefix}: could not detect dominant frequency (expected {freq}Hz)")

    res.ok = True
    return res


@dataclass
class StereoSplitResult:
    left_rms_db: float
    right_rms_db: float
    left_freq: Optional[float]
    right_freq: Optional[float]
    separated: bool
    checks: list = field(default_factory=list)
    ok: bool = False

    def to_dict(self) -> dict:
        return asdict(self)


def assert_stereo_split(
    wav_path,
    *,
    left_freq: Optional[float] = None,
    right_freq: Optional[float] = None,
    freq_tol_hz: float = 20.0,
    min_channel_rms_db: float = -45.0,
    label: str = "",
) -> StereoSplitResult:
    """Validate stereo recording channel separation (user vs agent on distinct channels).

    Fails if: file not stereo, either channel silent, or channel content does
    not match the expected per-channel tone frequencies.
    """
    p = Path(wav_path)
    if not p.exists():
        raise AssertionError(f"[stereo]{label} file missing: {p}")
    left, right, rate, channels = av.read_wav_stereo(str(p))
    if channels != 2:
        raise AssertionError(f"[stereo]{label} {p.name}: expected 2 channels, got {channels}")
    out = StereoSplitResult(
        left_rms_db=round(av.compute_rms_db(left), 2),
        right_rms_db=round(av.compute_rms_db(right), 2),
        left_freq=None,
        right_freq=None,
        separated=False,
    )
    for name, samples, rms in (("left", left, out.left_rms_db), ("right", right, out.right_rms_db)):
        ok = rms >= min_channel_rms_db
        out.checks.append({"name": f"{name}_rms_db", "expected": f">= {min_channel_rms_db}", "got": rms, "pass": ok})
        if not ok:
            raise AssertionError(f"[stereo]{label} {p.name}: {name} channel silent (rms={rms}dB)")
    if left_freq is not None:
        f, _ = av.find_dominant_frequency(left, rate)
        out.left_freq = round(f, 1) if f and not math.isnan(f) else None
        ok = out.left_freq is not None and abs(out.left_freq - left_freq) <= freq_tol_hz
        out.checks.append({"name": "left_freq_hz", "expected": f"{left_freq}±{freq_tol_hz}", "got": out.left_freq, "pass": ok})
        if not ok:
            raise AssertionError(f"[stereo]{label} {p.name}: left freq {out.left_freq} not {left_freq}±{freq_tol_hz}Hz")
    if right_freq is not None:
        f, _ = av.find_dominant_frequency(right, rate)
        out.right_freq = round(f, 1) if f and not math.isnan(f) else None
        ok = out.right_freq is not None and abs(out.right_freq - right_freq) <= freq_tol_hz
        out.checks.append({"name": "right_freq_hz", "expected": f"{right_freq}±{freq_tol_hz}", "got": out.right_freq, "pass": ok})
        if not ok:
            raise AssertionError(f"[stereo]{label} {p.name}: right freq {out.right_freq} not {right_freq}±{freq_tol_hz}Hz")
    out.ok = True
    return out


def assert_caller_audio(ua, rec_path, *, label: str = "", min_duration: float = 0.3, **kw) -> AudioCheckResult:
    """Strict caller-audio assertion with dual evidence channels.

    IVR/app-media legs have a WIP sipbot --record sink (0-byte / truncated /
    unparsable WAV at times — noted in the migrated e2e tests). This helper
    refuses to pass vacuously:

    * WAV parses with real duration -> full content checks (freq/RMS/duration)
      via :func:`assert_audio`;
    * WAV missing/empty/truncated (record-sink pathology, not a content fact)
      -> strict sipbot AudioQuality gates (has_audio / avg_rms / silence_ratio)
      which are real measured content signals.

    Either way a structured result lands in evidence. A parsed-but-wrong-content
    WAV is always a hard failure.
    """
    from pathlib import Path as _P

    p = _P(str(rec_path))
    size = p.stat().st_size if p.exists() else 0
    need = max(min_duration, 0.3)
    usable = False
    if size > 44:
        try:
            if _analyze(p, mono=True)[0].duration_sec is not None and _analyze(p, mono=True)[0].duration_sec >= need:
                usable = True
        except AssertionError:
            usable = False
    if usable:
        return assert_audio(p, label=label, min_duration=min_duration, **kw)

    q = ua.get_audio_quality() or {}
    res = AudioCheckResult(file=str(p), exists=p.exists(), bytes=size)
    res.checks.append({"name": "record_sink", "expected": "usable wav", 
                       "got": f"{size}B", "pass": False})
    has_audio = bool(q.get("has_audio"))
    avg_rms = q.get("avg_rms")
    silence = q.get("silence_ratio")
    gates = [
        ("has_audio", True, has_audio, has_audio),
        ("avg_rms", ">= 20", avg_rms, isinstance(avg_rms, (int, float)) and avg_rms >= 20),
        ("silence_ratio", "<= 0.55", silence, isinstance(silence, (int, float)) and silence <= 0.55),
    ]
    res.checks += [{"name": n, "expected": e, "got": g, "pass": ok} for (n, e, g, ok) in gates]
    bad = [c for c in res.checks if not c["pass"]]
    if bad:
        res.error = f"audio-quality gates failed: {bad}"
        raise AssertionError(
            f"[audio]{(' ' + label) if label else ''} quality gates failed: {q} "
            f"(record sink unusable: {size}B)"
        )
    res.ok = True
    return res
