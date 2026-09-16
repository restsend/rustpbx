#!/usr/bin/env bash
# Legacy entry point — forwards to the unified regression system.
#
# The full regression pipeline now lives in regression/run_full_e2e.sh
# (build-once + lane-parallel + evidence report). This shim keeps old
# invocations working.
#
# Old behaviour mapping:
#   ./scripts/run_full_regression.sh                -> unified `all`
#   ./scripts/run_full_regression.sh --skip-build   -> unified `all --skip-build`
#   ./scripts/run_full_regression.sh --only core    -> unified `core`
#   ./scripts/run_full_regression.sh --only cc      -> unified `addons-only --lane cc`
#   ./scripts/run_full_regression.sh --only cargo   -> `cargo` step only

set -uo pipefail
REG_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../regression" && pwd)"

ONLY=""
ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) ARGS+=("--skip-build"); shift ;;
    --only) ONLY="${2:-}"; shift 2 ;;
    *) ARGS+=("$1"); shift ;;
  esac
done

case "$ONLY" in
  core|wholesale|cc) MODE="addons-only"; LANE="$ONLY" ;;
  cargo|build) MODE="$ONLY"; LANE="" ;;
  *) MODE="all"; LANE="" ;;
esac

CMD=(bash "$REG_DIR/run_full_e2e.sh" "$MODE")
[[ -n "$LANE" ]] && CMD+=("--lane" "$LANE")
CMD+=("${ARGS[@]+"${ARGS[@]}"}")
exec "${CMD[@]}"
