#!/usr/bin/env bash
# Unified Python + sipbot E2E runner.
#
# Usage: ./run.sh [TIER] [-- pytest-args...]
#   ./run.sh                  # run all tests
#   ./run.sh fast             # everything except `slow`-marked tests
#   ./run.sh p2p              # only p2p-marked tests (marker shorthand)
#   ./run.sh -m "queue or ivr"# pass a marker expression straight through
#   ./run.sh all -- -n 2      # forward extra pytest args verbatim (e.g. -n for xdist)
#
# Env overrides:
#   PYTHON               python interpreter (default python3)
#   RUSTPBX_E2E_ADDONS   comma-separated addons (default: cc)
#   RUSTPBX_SIP_PORT     (default 15070)
#   RUSTPBX_HTTP_PORT    (default 18080)
#   RUSTPBX_E2E_REPORT_DIR
#   RUSTPBX_E2E_LOG_LEVEL

set -euo pipefail
cd "$(dirname "$0")"

PY="${PYTHON:-python3}"
TIER="${1:-all}"

# Everything after an explicit `--` is forwarded to pytest untouched.
if [[ "$TIER" == "--" ]]; then
  shift
  EXTRA=("$@")
  TIER="all"
else
  shift || true
  EXTRA=("$@")
fi

export RUSTPBX_E2E_REPORT_DIR="${RUSTPBX_E2E_REPORT_DIR:-$PWD/report}"
mkdir -p "$RUSTPBX_E2E_REPORT_DIR"

ARGS=(--tb=short --durations=15)
case "$TIER" in
  all|"") ;;
  fast) ARGS+=(-m "not slow") ;;
  -m)
    # Marker expression is the first positional, e.g. ./run.sh -m "queue or ivr"
    ARGS+=(-m "${EXTRA[0]:-}")
    if [[ ${#EXTRA[@]} -gt 1 ]]; then
      EXTRA=("${EXTRA[@]:1}")
    else
      EXTRA=()
    fi
    ;;
  -*) ARGS+=("$TIER") ;;
  *)  ARGS+=(-m "$TIER") ;;
esac

if "$PY" -c "import pytest_html" >/dev/null 2>&1; then
  ARGS+=(--html="$RUSTPBX_E2E_REPORT_DIR/index.html" --self-contained-html)
fi

echo "========================================"
echo " RustPBX unified E2E"
echo " Tier: $TIER  |  Report: $RUSTPBX_E2E_REPORT_DIR"
echo "========================================"

"$PY" -m pytest tests/ "${ARGS[@]}" ${EXTRA[@]+"${EXTRA[@]}"}
