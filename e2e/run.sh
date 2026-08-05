#!/usr/bin/env bash
# Unified Python + sipbot E2E runner.
#
# Usage: ./run.sh [marker...] [-- pytest-args]
#   ./run.sh                     # run all tests
#   ./run.sh p2p                 # only p2p-marked tests
#   ./run.sh -m "queue or ivr"
#
# Env overrides:
#   RUSTPBX_E2E_ADDONS   comma-separated addons (default: cc)
#   RUSTPBX_SIP_PORT     (default 15070)
#   RUSTPBX_HTTP_PORT    (default 18080)
#   RUSTPBX_E2E_REPORT_DIR
#   RUSTPBX_E2E_LOG_LEVEL

set -euo pipefail
cd "$(dirname "$0")"

PY="${PYTHON:-python3}"
MARK="${1:-}"
shift || true

export RUSTPBX_E2E_REPORT_DIR="${RUSTPBX_E2E_REPORT_DIR:-$PWD/report}"

if [[ -n "${MARK:-}" ]]; then
  exec "$PY" -m pytest tests/ -m "$MARK" "$@"
else
  exec "$PY" -m pytest tests/ "$@"
fi
