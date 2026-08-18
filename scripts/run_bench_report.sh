#!/usr/bin/env bash
# RustPBX benchmark + regression automation.
#
# One-shot pipeline: build → functional regression (p2p + wholesale) →
# performance (P2P bench) → memory leak → consolidated markdown report.
#
# Usage:
#   ./scripts/run_bench_report.sh                     # full run (build + all stages)
#   ./scripts/run_bench_report.sh --skip-build        # reuse existing binaries
#   ./scripts/run_bench_report.sh --skip-perf         # regression + memleak + report
#   ./scripts/run_bench_report.sh --skip-memleak      # regression + perf + report
#   ./scripts/run_bench_report.sh --only-regression   # regression only
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
#   RUSTPBX_E2E_ADDONS  e2e addons (default cc)
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
RUSTPBX_RELEASE="target/release/rustpbx"
RUSTPBX_E2E_BIN="target/debug/rustpbx-cc-e2e"

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
SKIP_REG=0
SKIP_PERF=0
SKIP_LEAK=0
ONLY_REG=0

usage() {
  sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

for arg in "$@"; do
  case "$arg" in
    --skip-build)  SKIP_BUILD=1 ;;
    --skip-regression) SKIP_REG=1 ;;
    --skip-perf)   SKIP_PERF=1 ;;
    --skip-memleak) SKIP_LEAK=1 ;;
    --only-regression) ONLY_REG=1 ;;
    -h|--help) usage 0 ;;
    *) echo "unknown option: $arg" >&2; usage 1 ;;
  esac
done

if [[ "$ONLY_REG" == "1" ]]; then
  SKIP_PERF=1
  SKIP_LEAK=1
fi

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
  step "build release binary (features: default,commerce,wholesale,contact-center)"
  export CC=clang RUST_MIN_STACK=1073741824
  cargo build --release --features default,commerce,wholesale,contact-center
  step "build e2e debug binary (features: default,contact-center,addon-sbc,addon-wholesale)"
  cargo build --features default,contact-center,addon-sbc,addon-wholesale
  cp target/debug/rustpbx "$RUSTPBX_E2E_BIN"
  log "✓ release: $RUSTPBX_RELEASE, e2e: $RUSTPBX_E2E_BIN"
}

# ---------------------------------------------------------------------------
# 2. Functional regression
# ---------------------------------------------------------------------------
run_regression() {
  mkdir -p "$REPORT_DIR"
  local reg_json="$REPORT_DIR/regression.json"
  local p2p_log="$REPORT_DIR/e2e_p2p.log"
  local ws_log="$REPORT_DIR/e2e_wholesale.log"
  local rust_log="$REPORT_DIR/wholesale_rust.log"

  step "regression: p2p (e2e, marker=p2p)"
  ( cd e2e && ./run.sh p2p ) 2>&1 | tee "$p2p_log" || true

  step "regression: wholesale e2e (marker=wholesale)"
  ( cd e2e && ./run.sh wholesale ) 2>&1 | tee "$ws_log" || true

  step "regression: wholesale Rust integration tests"
  export CC=clang
  cargo test --features default,wholesale,contact-center --test wholesale 2>&1 | tee "$rust_log" || true

  # --- parse summaries ---
  local p2p_total p2p_passed p2p_failed p2p_skipped p2p_secs
  local ws_total ws_passed ws_failed ws_skipped ws_secs
  local rust_passed rust_failed rust_ignored rust_secs

  # pytest: "... == N passed, M deselected ... in SS.SSs"
  p2p_line="$(grep -E '===+ .* passed' "$p2p_log" | tail -1 || true)"
  ws_line="$(grep -E '===+ .* passed' "$ws_log" | tail -1 || true)"

  p2p_passed="$(echo "$p2p_line" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' || echo 0)"
  p2p_failed="$(echo "$p2p_line" | grep -oE '[0-9]+ failed' | grep -oE '[0-9]+' || echo 0)"
  p2p_skipped="$(echo "$p2p_line" | grep -oE '[0-9]+ (skipped|deselected)' | grep -oE '[0-9]+' || echo 0)"
  p2p_secs="$(echo "$p2p_line" | grep -oE 'in [0-9.]+s' | grep -oE '[0-9.]+' || echo 0)"
  p2p_total=$((p2p_passed + p2p_failed))

  ws_passed="$(echo "$ws_line" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' || echo 0)"
  ws_failed="$(echo "$ws_line" | grep -oE '[0-9]+ failed' | grep -oE '[0-9]+' || echo 0)"
  ws_skipped="$(echo "$ws_line" | grep -oE '[0-9]+ (skipped|deselected)' | grep -oE '[0-9]+' || echo 0)"
  ws_secs="$(echo "$ws_line" | grep -oE 'in [0-9.]+s' | grep -oE '[0-9.]+' || echo 0)"
  ws_total=$((ws_passed + ws_failed))

  # cargo test: "test result: ok. 151 passed; 0 failed; ... finished in 23.58s"
  rust_line="$(grep -E '^test result:' "$rust_log" | tail -1 || true)"
  rust_passed="$(echo "$rust_line" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' || echo 0)"
  rust_failed="$(echo "$rust_line" | grep -oE '[0-9]+ failed' | grep -oE '[0-9]+' || echo 0)"
  rust_ignored="$(echo "$rust_line" | grep -oE '[0-9]+ ignored' | grep -oE '[0-9]+' || echo 0)"
  rust_secs="$(echo "$rust_line" | grep -oE 'in [0-9.]+s' | grep -oE '[0-9.]+' || echo 0)"

  cat > "$reg_json" <<EOF
{
  "p2p": {
    "passed": ${p2p_passed:-0}, "failed": ${p2p_failed:-0},
    "skipped": ${p2p_skipped:-0}, "total": ${p2p_total:-0},
    "duration_s": ${p2p_secs:-0}, "ok": $([ "${p2p_failed:-1}" == "0" ] && echo true || echo false)
  },
  "wholesale_e2e": {
    "passed": ${ws_passed:-0}, "failed": ${ws_failed:-0},
    "skipped": ${ws_skipped:-0}, "total": ${ws_total:-0},
    "duration_s": ${ws_secs:-0}, "ok": $([ "${ws_failed:-1}" == "0" ] && echo true || echo false)
  },
  "wholesale_rust": {
    "passed": ${rust_passed:-0}, "failed": ${rust_failed:-0},
    "ignored": ${rust_ignored:-0}, "total": $((rust_passed + rust_failed)),
    "duration_s": ${rust_secs:-0}, "ok": $([ "${rust_failed:-1}" == "0" ] && echo true || echo false)
  }
}
EOF
  log "✓ regression summary -> $reg_json"
}

# ---------------------------------------------------------------------------
# 3. Performance
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
# 4. Memory leak
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
# 5. Report
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

log "starting benchmark & regression automation"
log "  results dir : $REPORT_DIR"
log "  scenarios   : $SCENARIOS"
log "  perf        : total=$TOTAL cps=$CPS duration=${DURATION}s uas=$UAS_COUNT"
log "  memleak     : total=$LEAK_TOTAL batch=$LEAK_BATCH"
echo

preflight
build_all
[[ "$SKIP_REG" == "0" ]] && run_regression
[[ "$SKIP_PERF" == "0" ]] && run_perf
[[ "$SKIP_LEAK" == "0" ]] && run_memleak
gen_report

step "all done"
log "summary: $REPORT_DIR/report.md"
