#!/usr/bin/env bash
# RustPBX benchmark automation.
#
# One-shot pipeline: build → performance (P2P bench) → memory leak →
# consolidated markdown report.
#
# Usage:
#   ./scripts/run_bench_report.sh                     # full run (build + all stages)
#   ./scripts/run_bench_report.sh --skip-build        # reuse existing binaries
#   ./scripts/run_bench_report.sh --skip-perf         # memleak + report only
#   ./scripts/run_bench_report.sh --skip-memleak      # perf + report only
#   ./scripts/run_bench_report.sh --help
#
# Env overrides:
#   BENCH_RESULTS_DIR   results root (default tests/bench/results_auto)
#   BENCH_REPORT_DIR    report root (default $BENCH_RESULTS_DIR/<timestamp>)
#   BENCH_TOTAL         perf total calls (default 500)
#   BENCH_CPS           perf calls/sec (default 100)
#   BENCH_DURATION      perf call duration s (default 60)
#   BENCH_UAS_COUNT     perf UAS instances (default 2)
#   BENCH_SCENARIOS     comma-separated perf scenarios (default rtp_fastpath,rtp_fastpath_rec,webrtc_to_rtp)
#   BENCH_LEAK_TOTAL    memleak total calls (default 9000)
#   BENCH_LEAK_BATCH    memleak batch size (default 200)
#   BENCH_LOG_DIR       bench.py log dir (default $REPORT_DIR/bench)
#   RUSTPBX_RELEASE_FEATURES  release build features (default "default,commerce";
#                       a full deployment tree adds its own addon features here)
#   RUSTPBX_E2E_FEATURES      e2e debug build features (default "default,commerce")
#
# Requires: cargo, python3, sipbot on PATH, MySQL on 127.0.0.1:13306
# (bench config uses tests/bench/config_bench.toml).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PY="${PYTHON:-python3}"
BENCH_DIR="tests/bench"
BENCH_PY="$BENCH_DIR/bench.py"
REPORT_GEN="$BENCH_DIR/report_gen.py"
RUSTPBX_RELEASE="${RUSTPBX_RELEASE:-target/release/rustpbx}"
RUSTPBX_E2E_BIN="${RUSTPBX_E2E_BIN:-target/debug/rustpbx-e2e}"
RUSTPBX_RELEASE_FEATURES="${RUSTPBX_RELEASE_FEATURES:-default,commerce}"
RUSTPBX_E2E_FEATURES="${RUSTPBX_E2E_FEATURES:-default,commerce}"

# --- Defaults ---------------------------------------------------------------
TOTAL="${BENCH_TOTAL:-500}"
CPS="${BENCH_CPS:-100}"
DURATION="${BENCH_DURATION:-60}"
UAS_COUNT="${BENCH_UAS_COUNT:-2}"
SCENARIOS="${BENCH_SCENARIOS:-rtp_fastpath,rtp_fastpath_rec,webrtc_to_rtp}"
LEAK_TOTAL="${BENCH_LEAK_TOTAL:-9000}"
LEAK_BATCH="${BENCH_LEAK_BATCH:-200}"
RESULTS_DIR="${BENCH_RESULTS_DIR:-$BENCH_DIR/results_auto}"

# --- Flags ------------------------------------------------------------------
SKIP_BUILD=0
SKIP_PERF=0
SKIP_LEAK=0

usage() {
  sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

for arg in "$@"; do
  case "$arg" in
    --skip-build)  SKIP_BUILD=1 ;;
    --skip-perf)   SKIP_PERF=1 ;;
    --skip-memleak) SKIP_LEAK=1 ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $arg" >&2; usage 1 ;;
  esac
done

# --- Helpers ----------------------------------------------------------------
now() { date '+%Y-%m-%d %H:%M:%S'; }
log() { printf '[%s] %s\n' "$(now)" "$*"; }
step() { printf '\n\033[1;36m==== %s ====\033[0m\n' "$*"; }

preflight() {
  local ok=1
  command -v "$PY" >/dev/null 2>&1 || { log "❌ python3 not found"; ok=0; }
  command -v sipbot >/dev/null 2>&1 || { log "❌ sipbot not found (cargo install sipbot)"; ok=0; }
  if [[ "$SKIP_PERF" == "0" || "$SKIP_LEAK" == "0" ]]; then
    # bench.py config uses MySQL at 127.0.0.1:13306 (docker rs_db)
    if ! (echo > /dev/tcp/127.0.0.1/13306) 2>/dev/null; then
      log "❌ MySQL 127.0.0.1:13306 unreachable (bench needs it; docker rs_db)"
      ok=0
    fi
  fi
  if [[ "$SKIP_BUILD" == "0" ]]; then
    command -v cargo >/dev/null 2>&1 || { log "❌ cargo not found"; ok=0; }
  fi
  if [[ "$ok" == "0" ]]; then
    log "preflight failed"
    exit 1
  fi
  log "✓ preflight OK"
}

# ---------------------------------------------------------------------------
# 1. Build
# ---------------------------------------------------------------------------
build_all() {
  if [[ "$SKIP_BUILD" == "1" ]]; then
    [[ -x "$RUSTPBX_RELEASE" ]] || { log "❌ $RUSTPBX_RELEASE missing (use --skip-build only after building)"; return 1; }
    [[ -x "$RUSTPBX_E2E_BIN" ]] || { log "❌ $RUSTPBX_E2E_BIN missing (use --skip-build only after building)"; return 1; }
    log "skip build (reusing existing binaries)"
    return 0
  fi
  step "build release binary (features: $RUSTPBX_RELEASE_FEATURES)"
  export CC=clang RUST_MIN_STACK=1073741824
  cargo build --release --features "$RUSTPBX_RELEASE_FEATURES"
  step "build e2e debug binary (features: $RUSTPBX_E2E_FEATURES)"
  cargo build --features "$RUSTPBX_E2E_FEATURES"
  cp target/debug/rustpbx "$RUSTPBX_E2E_BIN"
  log "✓ release: $RUSTPBX_RELEASE, e2e: $RUSTPBX_E2E_BIN"
}

# ---------------------------------------------------------------------------
# 2. Performance
# ---------------------------------------------------------------------------
run_perf() {
  step "performance test (scenarios: $SCENARIOS, total=$TOTAL cps=$CPS duration=${DURATION}s uas=$UAS_COUNT)"
  IFS=',' read -ra SCN <<< "$SCENARIOS"
  for sc in "${SCN[@]}"; do
    sc="${sc// /}"
    [[ -n "$sc" ]] || continue
    log ">> scenario $sc"
    mkdir -p "$REPORT_DIR/perf/$sc"
    if ! $PY "$BENCH_PY" \
        --scenario "$sc" \
        --total "$TOTAL" --cps "$CPS" --duration "$DURATION" --uas-count "$UAS_COUNT" \
        --log-dir "$REPORT_DIR/perf/$sc"; then
      log "! scenario $sc failed (non-fatal)"
    fi
  done
}

# ---------------------------------------------------------------------------
# 3. Memory leak
# ---------------------------------------------------------------------------
run_memleak() {
  step "memory leak test (total=$LEAK_TOTAL batch=$LEAK_BATCH cps=$CPS duration=${DURATION}s)"
  mkdir -p "$REPORT_DIR/memleak"
  $PY "$BENCH_PY" --memleak \
      --total "$LEAK_TOTAL" --batch-size "$LEAK_BATCH" \
      --cps "$CPS" --duration "$DURATION" --uas-count "$UAS_COUNT" \
      --log-dir "$REPORT_DIR/memleak"
}

# ---------------------------------------------------------------------------
# 4. Report
# ---------------------------------------------------------------------------
gen_report() {
  step "generate report"
  export BENCH_GIT_COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo -)"
  export BENCH_GIT_BRANCH="$(git branch --show-current 2>/dev/null || echo -)"
  export BENCH_HOST="$(hostname 2>/dev/null || echo -)"
  export BENCH_CPU="$(nproc 2>/dev/null || echo -)"
  export BENCH_SIPBOT="$(sipbot --version 2>/dev/null || echo -)"
  export BENCH_RUSTPBX_BIN="$RUSTPBX_RELEASE"
  $PY "$REPORT_GEN" "$REPORT_DIR" --out "$REPORT_DIR/report.md"
  log "report: $REPORT_DIR/report.md"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
TS="$(date '+%Y%m%d_%H%M%S')"
REPORT_DIR="${BENCH_REPORT_DIR:-$RESULTS_DIR/$TS}"
mkdir -p "$REPORT_DIR"

log "starting benchmark automation"
log "  results dir : $REPORT_DIR"
log "  scenarios   : $SCENARIOS"
log "  perf        : total=$TOTAL cps=$CPS duration=${DURATION}s uas=$UAS_COUNT"
log "  memleak     : total=$LEAK_TOTAL batch=$LEAK_BATCH"
echo

preflight
build_all
[[ "$SKIP_PERF" == "0" ]] && run_perf
[[ "$SKIP_LEAK" == "0" ]] && run_memleak
gen_report

step "all done"
log "summary: $REPORT_DIR/report.md"
