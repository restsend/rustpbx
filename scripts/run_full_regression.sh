#!/usr/bin/env bash
# Full regression with scenario separation (CC-core vs wholesale vs CC suite).
#
# Why scenarios are split:
#   - core/CC file routes use DefaultRouteInvite (IVR/queue/p2p/...).
#   - wholesale installs WholesaleRouteInvite which *replaces* default routing.
#     Enabling wholesale in the default fixture makes IVR/queue look "offline"
#     (SIP 480). Wholesale tests must opt in via ConfigBuilder.set_wholesale().
#   - CC e2e-regression is a separate pytest tree under src/addons/cc.
#
# Usage:
#   ./scripts/run_full_regression.sh              # build + all scenarios
#   ./scripts/run_full_regression.sh --skip-build
#   ./scripts/run_full_regression.sh --only core
#   ./scripts/run_full_regression.sh --only wholesale
#   ./scripts/run_full_regression.sh --only cc
#   ./scripts/run_full_regression.sh --only cargo
#
# Env:
#   RUSTPBX_SIP_PORT / RUSTPBX_HTTP_PORT  (defaults 16070/19080 to avoid demos on 15070)
#   RUSTPBX_E2E_ADDONS                    (core only; wholesale is stripped)

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

FEATURES="${RUSTPBX_REGRESSION_FEATURES:-commerce,wholesale,contact-center}"
export RUSTPBX_SIP_PORT="${RUSTPBX_SIP_PORT:-16070}"
export RUSTPBX_HTTP_PORT="${RUSTPBX_HTTP_PORT:-19080}"
# Core fixture addons — never include wholesale here.
export RUSTPBX_E2E_ADDONS="${RUSTPBX_E2E_ADDONS:-cc}"

LOG_DIR="$REPO_ROOT/tests/logs"
mkdir -p "$LOG_DIR"
STAMP="$(date +%Y%m%d_%H%M%S)"
MASTER="$LOG_DIR/full_regression_${STAMP}.log"
SUMMARY="$LOG_DIR/full_regression_${STAMP}.summary"

SKIP_BUILD=0
ONLY=""

usage() {
  sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1 ;;
    --only) ONLY="${2:-}"; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $1" >&2; usage 1 ;;
  esac
  shift
done

exec > >(tee -a "$MASTER") 2>&1

echo "===== FULL REGRESSION START $(date -Is) ====="
echo "repo=$(git rev-parse --short HEAD) branch=$(git branch --show-current)"
echo "features=$FEATURES"
echo "core addons=$RUSTPBX_E2E_ADDONS  ports SIP=$RUSTPBX_SIP_PORT HTTP=$RUSTPBX_HTTP_PORT"
echo "log=$MASTER"
: > "$SUMMARY"

fail=0
run_step() {
  local name="$1"; shift
  echo
  echo "######## STEP: $name ########"
  local start end dur rc
  start=$(date +%s)
  if "$@"; then rc=0; else rc=$?; fi
  end=$(date +%s)
  dur=$((end - start))
  if [[ $rc -eq 0 ]]; then
    echo "STEP_OK $name (${dur}s)" | tee -a "$SUMMARY"
  else
    echo "STEP_FAIL $name rc=$rc (${dur}s)" | tee -a "$SUMMARY"
    fail=1
  fi
  return 0
}

should_run() {
  local name="$1"
  [[ -z "$ONLY" || "$ONLY" == "$name" ]]
}

if should_run build && [[ "$SKIP_BUILD" == "0" ]]; then
  run_step build cargo build --features "$FEATURES"
  if [[ -x target/debug/rustpbx ]]; then
    cp -f target/debug/rustpbx target/debug/rustpbx-cc-e2e
    echo "copied rustpbx -> rustpbx-cc-e2e"
  fi
fi

if should_run cargo; then
  # --no-fail-fast so lib failures do not skip integration binaries
  run_step cargo_test cargo test --features "$FEATURES" --no-fail-fast -- --nocapture
fi

if should_run core; then
  # CC-core / file routes — explicitly exclude wholesale marker + addon
  run_step e2e_core bash -lc 'cd e2e && ./run.sh core'
fi

if should_run wholesale; then
  run_step e2e_wholesale bash -lc 'cd e2e && ./run.sh wholesale'
fi

if should_run cc; then
  run_step e2e_cc bash -lc 'cd src/addons/cc/e2e-regression && ./run.sh all'
fi

echo
echo "===== FULL REGRESSION END $(date -Is) fail=$fail ====="
echo "SUMMARY:"
cat "$SUMMARY"
echo "MASTER_LOG=$MASTER"
exit "$fail"
