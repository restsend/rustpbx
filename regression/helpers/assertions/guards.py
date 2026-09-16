"""Fail-fast guards: assert data exists and is non-empty *before* using it.

Every regression assertion must operate on real, populated artifacts. A missing
CDR file, an empty event buffer, or a null field must fail immediately with a
message that names the missing artifact — never with an IndexError, a silent
skip, or a vacuous pass.
"""

from __future__ import annotations

from typing import Any, Iterable, Optional


def require(value: Any, what: str, hint: str = "") -> Any:
    """Assert *value* is not None, else AssertionError naming *what*."""
    if value is None:
        msg = f"[guard] required {what} is None"
        if hint:
            msg += f" ({hint})"
        raise AssertionError(msg)
    return value


def require_non_empty(value: Any, what: str, hint: str = "") -> Any:
    """Assert *value* is not None and has content (len>0 for sized types)."""
    require(value, what, hint)
    try:
        if len(value) == 0:
            msg = f"[guard] required {what} is empty"
            if hint:
                msg += f" ({hint})"
            raise AssertionError(msg)
    except TypeError:
        pass  # non-sized scalars (int/float/bool) only need the None check
    return value


def require_file(path, what: str, min_bytes: int = 1, hint: str = ""):
    """Assert *path* exists and is at least *min_bytes* (catches empty artifacts)."""
    from pathlib import Path

    p = Path(path)
    if not p.exists():
        raise AssertionError(f"[guard] required file {what} missing: {p} {hint}")
    size = p.stat().st_size
    if size < min_bytes:
        raise AssertionError(
            f"[guard] required file {what} too small: {p} has {size} bytes (< {min_bytes}) {hint}"
        )
    return p


def require_keys(mapping: Any, keys: Iterable[str], what: str) -> dict:
    """Assert *mapping* is a dict containing all *keys* (values may be any)."""
    require(mapping, what)
    if not isinstance(mapping, dict):
        raise AssertionError(f"[guard] {what} is not a dict: {type(mapping).__name__} — {mapping!r:.200}")
    missing = [k for k in keys if k not in mapping]
    if missing:
        raise AssertionError(
            f"[guard] {what} missing required keys {missing} — got keys: {sorted(mapping.keys())}"
        )
    return mapping


def forbid(*, none_of: Iterable[Any] = (), empty_of: Iterable[Any] = (), what: str = "value"):
    """Negative guard: values must not be in *none_of* nor empty in *empty_of*."""
    for label, values, bad in (("must not be", none_of, (None,)), ("must not be empty", empty_of, None)):
        for i, v in enumerate(values):
            if bad == (None,):
                if v is None:
                    raise AssertionError(f"[guard] {what}[{i}] {label} None")
            else:
                try:
                    n = len(v)
                except TypeError:
                    continue
                if n == 0:
                    raise AssertionError(f"[guard] {what}[{i}] {label}")


def diff_map(expected: dict, got: dict, what: str = "data") -> str:
    """Return a human-readable diff of expected-vs-got scalar fields (for evidence)."""
    lines = [f"--- {what} diff ---"]
    for k, want in expected.items():
        have = got.get(k, "<MISSING>")
        mark = "OK " if _loose_eq(want, have) else "MISMATCH"
        lines.append(f"  [{mark}] {k}: expected={want!r} got={have!r}")
    extra = [k for k in got.keys() if k not in expected]
    if extra:
        lines.append(f"  [INFO] fields only in got: {extra}")
    return "\n".join(lines)


def _loose_eq(a, b) -> bool:
    """Equality tolerant to int/float str-coercion and substring match for str/str."""
    if a == b:
        return True
    if isinstance(a, str) and isinstance(b, str):
        return a in b or b in a
    try:
        return float(a) == float(b)
    except (TypeError, ValueError):
        return False
