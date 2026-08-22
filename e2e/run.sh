#!/usr/bin/env bash
# Unified Python + sipbot E2E runner.
#
# Usage: ./run.sh [SCENARIO] [-- pytest-args...]
#   ./run.sh                  # alias of `scenarios` (recommended)
#   ./run.sh scenarios        # run separated scenarios sequentially
#   ./run.sh core             # CC-core / file routes: IVR, queue, p2p, ...
#                             #   (excludes wholesale; addons default to cc)
#   ./run.sh wholesale        # wholesale billing only (set_wholesale per test)
#   ./run.sh fast             # core + not slow
#   ./run.sh p2p              # marker shorthand (still CC-core addons)
#   ./run.sh -m "queue or ivr"
#   ./run.sh all              # single pytest session over every test
#                             #   (wholesale still stripped from default addons)
#   ./run.sh all -- -n 2      # forward extra pytest args (keep -n 1 for PBX ports)
#
# Scenarios (why they are split):
#   core       — DefaultRouteInvite + file routes (IVR/queue/app). Runtime
#                addons: RUSTPBX_E2E_ADDONS or "cc". Wholesale is never default.
#   wholesale  — WholesaleRouteInvite replaces default routing; only tests that
#                call ConfigBuilder.set_wholesale() belong here.
#   cc suite   — separate tree: src/addons/cc/e2e-regression/ (not this script)
#
# Env overrides:
#   PYTHON               python interpreter (default python3)
#   RUSTPBX_E2E_ADDONS   comma-separated addons for core (default: cc)
#                        "wholesale" in this list is ignored for core fixtures
#   RUSTPBX_SIP_PORT     (default 15070)
#   RUSTPBX_HTTP_PORT    (default 18080)
#   RUSTPBX_E2E_WORKERS  pytest-xdist workers (default 2; each worker gets its
#                        own port range + artifact dir). Set 1 to run serially.
#   RUSTPBX_E2E_SCENARIOS_PARALLEL  "1" to run core+wholesale pytest sessions
#                        CONCURRENTLY in `scenarios` mode (the wholesale
#                        session gets a +20000 port base). Default: sequential.
#   RUSTPBX_E2E_BIN      prebuilt feature-complete binary (skips any build)
#   RUSTPBX_E2E_REPORT_DIR
#   RUSTPBX_E2E_LOG_LEVEL

set -euo pipefail
cd "$(dirname "$0")"

PY="${PYTHON:-python3}"
TIER="${1:-scenarios}"
WORKERS="${RUSTPBX_E2E_WORKERS:-2}"

if [[ "$TIER" == "-h" || "$TIER" == "--help" ]]; then
  sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
fi

# Everything after an explicit `--` is forwarded to pytest untouched.
if [[ "$TIER" == "--" ]]; then
  shift
  EXTRA=("$@")
  TIER="scenarios"
else
  shift || true
  EXTRA=("$@")
fi
# Drop a leading `--` so EXTRA is pure pytest args / node ids.
if [[ ${#EXTRA[@]} -gt 0 && "${EXTRA[0]}" == "--" ]]; then
  EXTRA=("${EXTRA[@]:1}")
fi

export RUSTPBX_E2E_REPORT_DIR="${RUSTPBX_E2E_REPORT_DIR:-$PWD/report}"
mkdir -p "$RUSTPBX_E2E_REPORT_DIR"

run_pytest() {
  local label="$1"
  shift
  local args=(${@+"$@"})
  local report_html="$RUSTPBX_E2E_REPORT_DIR/${label}.html"
  local pytest_args=(--tb=short --durations=15)

  if [[ "$WORKERS" -gt 1 ]]; then
    if "$PY" -c "import xdist" >/dev/null 2>&1; then
      pytest_args+=(-n "$WORKERS")
    else
      echo "WARN: RUSTPBX_E2E_WORKERS=$WORKERS but pytest-xdist not installed; running serially" >&2
    fi
  fi

  pytest_args+=(${args[@]+"${args[@]}"})

  if "$PY" -c "import pytest_html" >/dev/null 2>&1; then
    pytest_args+=(--html="$report_html" --self-contained-html)
  fi

  echo "========================================"
  echo " RustPBX unified E2E — scenario: $label"
  echo " Addons: ${RUSTPBX_E2E_ADDONS:-cc (default)}"
  echo " Report: $report_html"
  echo "========================================"

  # If the caller passed explicit test paths after `--`, don't also collect
  # the whole `tests/` tree (that would ignore the selection intent).
  local roots=("tests/")
  if [[ ${#EXTRA[@]} -gt 0 ]]; then
    local has_path=0
    local e
    for e in "${EXTRA[@]}"; do
      [[ "$e" == "--" ]] && continue
      if [[ "$e" == tests/* || "$e" == */tests/* || "$e" == *.py || "$e" == *.py::* ]]; then
        has_path=1
        break
      fi
    done
    if [[ "$has_path" == "1" ]]; then
      roots=()
    fi
  fi

  "$PY" -m pytest ${roots[@]+"${roots[@]}"} "${pytest_args[@]}" ${EXTRA[@]+"${EXTRA[@]}"}
}

# Core PBX: never enable wholesale at the fixture level.
ensure_core_addons() {
  local raw="${RUSTPBX_E2E_ADDONS:-cc}"
  # Drop wholesale if a caller exported a mixed list by mistake.
  local cleaned
  cleaned="$(echo "$raw" | tr ',' '\n' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' | grep -v '^$' | grep -vx 'wholesale' | paste -sd, - || true)"
  export RUSTPBX_E2E_ADDONS="${cleaned:-cc}"
}

case "$TIER" in
  scenarios|scenario|"")
    fail=0
    ensure_core_addons
    if [[ "${RUSTPBX_E2E_SCENARIOS_PARALLEL:-0}" == "1" ]]; then
      # Both pytest sessions concurrently; the wholesale session lives in a
      # +20000 port window (RUSTPBX_E2E_PORT_BASE shifts SIP/HTTP and every
      # fixed UA port, incl. worker 0).
      run_pytest core -m "not wholesale" &
      core_pid=$!
      RUSTPBX_E2E_PORT_BASE=20000 run_pytest wholesale -m wholesale &
      whole_pid=$!
      wait "$core_pid" || fail=1
      wait "$whole_pid" || fail=1
    else
      run_pytest core -m "not wholesale" || fail=1
      # Wholesale tests call set_wholesale() themselves; keep default addons clean.
      ensure_core_addons
      run_pytest wholesale -m wholesale || fail=1
    fi
    exit "$fail"
    ;;
  core|pbx|cc-core)
    ensure_core_addons
    run_pytest core -m "not wholesale"
    ;;
  wholesale)
    ensure_core_addons
    run_pytest wholesale -m wholesale
    ;;
  fast)
    ensure_core_addons
    run_pytest fast -m "not wholesale and not slow"
    ;;
  all)
    ensure_core_addons
    run_pytest all
    ;;
  -m)
    ensure_core_addons
    # Marker expression is the first positional, e.g. ./run.sh -m "queue or ivr"
    mark_expr="${EXTRA[0]:-}"
    if [[ ${#EXTRA[@]} -gt 1 ]]; then
      EXTRA=("${EXTRA[@]:1}")
    else
      EXTRA=()
    fi
    run_pytest custom -m "$mark_expr"
    ;;
  -*)
    ensure_core_addons
    run_pytest custom "$TIER"
    ;;
  *)
    # Marker shorthand: p2p / ivr / queue / sbc / voicemail / ...
    ensure_core_addons
    run_pytest "$TIER" -m "$TIER"
    ;;
esac
