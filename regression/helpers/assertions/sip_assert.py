"""Strict SIP signaling assertions: status-code sequences and DTMF digit flows.

sipbot prints `Status: {code:count,...}` lines (parsed by helpers.sipbot into
dicts); DTMF arrives via `RX_DTMF_DIGIT:` lines and get_dtmf_digits(). These
assertions demand *exact* sequences — "a 200 somewhere" is not a pass.
"""

from __future__ import annotations

from typing import Iterable, Optional

from .guards import require


def _collect_codes(*outputs: str) -> list[int]:
    """Extract status codes in appearance order from sipbot output(s)."""
    import re

    codes: list[int] = []
    for out in outputs:
        require(out is not None, "sipbot output")
        for m in re.finditer(r"Status:\s*\{([^}]*)\}", out):
            for pair in m.group(1).split(","):
                if ":" in pair:
                    try:
                        codes.append(int(pair.split(":")[0].strip()))
                    except ValueError:
                        continue
    return codes


def assert_sip_answered(*outputs: str, provisional: Optional[Iterable[int]] = None) -> list[int]:
    """Require exact sequence: any of *provisional* (default 180/183) then 200.

    Returns the observed code sequence for evidence.
    """
    provisional = set(provisional) if provisional else {180, 183}
    codes = _collect_codes(*outputs)
    require(codes, "SIP status codes", "no Status lines parsed — call may never have progressed")
    try:
        idx200 = len(codes) - 1 - codes[::-1].index(200)
    except ValueError:
        raise AssertionError(f"[sip] no 200 OK in observed sequence {codes}")
    prefix = [c for c in codes[:idx200]]
    if not any(c in provisional for c in prefix):
        raise AssertionError(
            f"[sip] answered without provisional response: expected one of "
            f"{sorted(provisional)} before 200, got {codes}"
        )
    return codes


def assert_sip_rejected(*outputs: str, expected_code: Optional[int] = None) -> list[int]:
    """Require final failure code (4xx/5xx/486/603...), optionally an exact one."""
    codes = _collect_codes(*outputs)
    require(codes, "SIP status codes", "no Status lines parsed")
    if expected_code is not None:
        if expected_code not in codes:
            raise AssertionError(f"[sip] expected rejection {expected_code} not in observed {codes}")
        if 200 in codes:
            raise AssertionError(f"[sip] call unexpectedly answered (200 in {codes}) but expected {expected_code}")
        return codes
    final = codes[-1]
    if final >= 200 and final < 300:
        raise AssertionError(f"[sip] expected rejection but last code was 2xx: {codes}")
    return codes


def assert_sip_no_answer(*outputs: str) -> list[int]:
    """Require the call never reached 200 (e.g. CANCEL/408 flows)."""
    codes = _collect_codes(*outputs)
    require(codes, "SIP status codes")
    if 200 in codes:
        raise AssertionError(f"[sip] 200 OK present but call must not be answered: {codes}")
    return codes


def assert_dtmf_digits(received: Optional[Iterable[str]], expected: str, *, label: str = "") -> list[str]:
    """Require received DTMF digits to equal *expected* exactly (order + count).

    Empty/None received digits is a hard failure — no "no digits, fine".
    """
    got = list(received) if received else []
    want = list(expected)
    if got != want:
        raise AssertionError(
            f"[dtmf]{(' ' + label) if label else ''} digits mismatch: expected {want} got {got}"
        )
    return got


def codes_summary(codes: list[int]) -> dict:
    """Compact evidence dict {code: count} from an ordered code list."""
    out: dict[str, int] = {}
    for c in codes:
        out[str(c)] = out.get(str(c), 0) + 1
    return out
