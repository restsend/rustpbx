#!/usr/bin/env python3
"""Assertion-quality lint for the e2e-regression test suite.

Flags test patterns that cannot catch functional regressions (tautologies,
silent passes, over-broad status sets). See TEST_ACCEPTANCE_AUDIT.md §2 for
the rationale behind each rule.

Usage:
    python scripts/check_assertion_quality.py [--strict] [--baseline FILE]

Exit codes: 0 = no NEW violations (or none at all), 1 = new violations found.

The baseline file lists known violations (``file:line:rule``) so the gate
starts green and ratchets to zero as phases 1-3 land. ``--strict`` ignores
the baseline and fails on any violation.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TESTS = ROOT / "tests"

RULES = {
    # assert X or len(...) > 0  — the or-leg makes the assert unfailable
    "or-len-tautology": re.compile(
        r"assert\s+.*\bor\s+len\([^)]*\)\s*(?:>=|>)\s*0", re.X
    ),
    # assert anything >= 0 — always true
    "ge-zero-tautology": re.compile(r"assert\s+[^#\n]*>=\s*0\b"),
    # bare `assert x.output` — sipbot always prints something
    "bare-output-assert": re.compile(r"assert\s+\w+\.output\b(?!\.)"),
    # except-then-pass around the behavior under test
    "except-pass": None,  # multi-line, handled separately
    # status sets that accept 404/500 alongside 200 on happy paths
    "status-set-404-500": re.compile(
        r"(?:status|s)\s*(?:==|in)?\s*[\[(][^\]\n]*(?:404|500)[^\]\n]*[\])]"
        r"|[\"'](200[^\"']*(?:404|500)|(?:404|500)[^\"']*200)[\"']"
    ),
    # wait_* helpers whose return value is discarded (silent pass)
    "ignored-wait-return": re.compile(
        r"^\s*(?:await\s+)?(?:self\.)?(?:\w+[._])?wait_for_(?:min_)?events?\("
        r"|^\s*(?:await\s+)?\w+\.wait_output_async\("
        r"|^\s*(?:ok|answered|result|ev|res)?\s*=?\s*(?:await\s+)?\w+\.wait_output_async\("
    ),
    # wide SIP regexes that match the 407 challenge present in EVERY call
    "wide-sip-regex": re.compile(
        r"[rR][\"'](?:[^\"']*\|)*(?:INVITE|SIP|407|\"4\")(?:\|[^\"']*)*[\"']"
    ),
}


def iter_test_files():
    suites = ROOT / "suites"
    if suites.is_dir():
        for p in sorted(suites.rglob("test_*.py")):
            yield p
    else:  # legacy layout fallback
        for tier in ("acceptance", "tier1", "tier2", "tier3"):
            d = TESTS / tier
            if not d.is_dir():
                continue
            for p in sorted(d.glob("*.py")):
                if p.name == "__init__.py":
                    continue
                yield p


def find_violations(path: Path) -> list[str]:
    out: list[str] = []
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    rel = path.relative_to(ROOT).as_posix()

    for idx, line in enumerate(lines, start=1):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if "NOQA: assert-quality" in line:
            continue

        for rule, rx in RULES.items():
            if rx is None:
                continue
            # wait-return rule only fires when nothing captures the result
            if rule == "ignored-wait-return":
                if "=" in stripped.split("wait", 1)[0]:
                    continue  # result is being assigned
                if re.match(r"^\s*(?:ok|answered|result|ev|res)\s*=", stripped):
                    continue
            if rx.search(line):
                out.append(f"{rel}:{idx}:{rule}")

        # except ... : pass (two-line form)
        if re.match(r"^\s*except\b.*:\s*$", line):
            nxt = lines[idx] if idx < len(lines) else ""
            if re.match(r"^\s*pass\s*$", nxt):
                out.append(f"{rel}:{idx}:except-pass")

    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--strict", action="store_true",
                    help="ignore the baseline; fail on any violation")
    ap.add_argument("--baseline", default=str(ROOT / "scripts" / "assertion_baseline.txt"))
    ap.add_argument("--update-baseline", action="store_true",
                    help="rewrite the baseline with current findings")
    args = ap.parse_args()

    findings: list[str] = []
    for f in iter_test_files():
        findings.extend(find_violations(f))

    baseline_path = Path(args.baseline)
    baseline = set()
    if baseline_path.exists() and not args.update_baseline:
        baseline = {
            ln.strip()
            for ln in baseline_path.read_text().splitlines()
            if ln.strip() and not ln.startswith("#")
        }

    if args.update_baseline:
        baseline_path.write_text(
            "# Assertion-quality baseline — ratchet to zero (see TEST_ACCEPTANCE_AUDIT.md)\n"
            + "\n".join(sorted(findings))
            + "\n"
        )
        print(f"baseline updated: {len(findings)} entries")
        return 0

    new = [f for f in findings if args.strict or f not in baseline]
    fixed = sorted(baseline - set(findings))

    for f in new:
        print(f"NEW  {f}")
    for f in fixed:
        print(f"FIXED {f}")

    total = len(findings) if args.strict else len(new)
    print(f"\nfindings={len(findings)} baseline={len(baseline)} new={len(new)} fixed={len(fixed)}")
    if new:
        print("FAIL: new assertion-quality violations (see rules in TEST_ACCEPTANCE_AUDIT.md §P0.2)")
        return 1
    print("OK: no new assertion-quality violations")
    return 0


if __name__ == "__main__":
    sys.exit(main())
