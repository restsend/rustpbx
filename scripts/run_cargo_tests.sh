#!/usr/bin/env bash
# Run every cargo test suite, serially (baseline) or with binaries in parallel.
#
#   scripts/run_cargo_tests.sh --list            # list the test binaries and exit
#   scripts/run_cargo_tests.sh --serial          # baseline: one binary after another
#   scripts/run_cargo_tests.sh --parallel        # run binaries concurrently (default N = nproc/2)
#   scripts/run_cargo_tests.sh --parallel 3      # explicit concurrency
#   scripts/run_cargo_tests.sh --serial --features commerce,wholesale,contact-center
#
# Build happens FIRST via cargo (`--no-run --message-format=json`) so cargo's
# build lock is never contended; the test executables are then invoked directly
# from target/debug/deps/. Ports in the suites are randomized via portpicker,
# so binaries are safe to run concurrently.
#
# Output: per-binary wall time + status, total wall time, log per binary under
# target/test-logs/<run_id>/summary.txt.
set -uo pipefail

MODE="parallel"
JOBS=""
FEATURES="commerce,wholesale,contact-center"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --serial) MODE="serial"; shift ;;
    --parallel)
      MODE="parallel"; shift
      if [[ $# -gt 0 && "$1" =~ ^[0-9]+$ ]]; then JOBS="$1"; shift; fi
      ;;
    --features) FEATURES="$2"; shift 2 ;;
    --list) MODE="list"; shift ;;
    -h|--help)
      sed -n '2,17p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown arg: $1 (see --help)" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RUN_ID="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${RUSTPBX_TEST_LOG_DIR:-$ROOT/target/test-logs/$RUN_ID}"
mkdir -p "$LOG_DIR"

echo "==> cargo test --no-run --features $FEATURES"
BUILD_START="$(date +%s)"
CARGO_TERM_COLOR=never cargo test --workspace --no-run --features "$FEATURES" \
  --message-format=json > "$LOG_DIR/build.jsonl" 2> "$LOG_DIR/build.stderr"
BUILD_RC=$?
BUILD_END="$(date +%s)"
echo "==> build: $((BUILD_END - BUILD_START))s (log: $LOG_DIR/build.stderr)"
if [[ $BUILD_RC -ne 0 ]]; then
  tail -30 "$LOG_DIR/build.stderr" >&2
  exit "$BUILD_RC"
fi

# Extract test executables: compiler-artifact messages with an executable and
# profile.test == true (lib unittest binaries + every integration test binary).
# (read loop instead of mapfile: macOS ships bash 3.2)
BINARIES=()
while IFS= read -r line; do
  [[ -n "$line" ]] && BINARIES+=("$line")
done < <(python3 - "$LOG_DIR/build.jsonl" <<'PY'
import json, sys
out = []
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    if msg.get("reason") != "compiler-artifact":
        continue
    exe = msg.get("executable")
    if not exe or not msg.get("profile", {}).get("test"):
        continue
    if exe not in out:
        out.append(exe)
print("\n".join(out))
PY
)

if [[ ${#BINARIES[@]} -eq 0 ]]; then
  echo "no test binaries found" >&2
  exit 1
fi

# Display name: dep binaries are <name>-<16 hex>; strip the hash suffix.
name_of() { basename "$1" | sed -E 's/-[0-9a-f]{16}$//'; }

if [[ "$MODE" == "list" ]]; then
  printf '%s\n' "${BINARIES[@]}" | while read -r b; do name_of "$b"; done
  exit 0
fi

if [[ -z "$JOBS" ]]; then
  JOBS=$(( $(sysctl -n hw.ncpu 2>/dev/null || nproc) / 2 ))
  [[ "$JOBS" -lt 1 ]] && JOBS=1
fi

declare -a NAMES=()
for b in "${BINARIES[@]}"; do NAMES+=("$(name_of "$b")"); done

# --- runners -----------------------------------------------------------------
declare -a FAILURES=()
TOTAL_START="$(date +%s)"

run_one() { # $1=binary $2=name $3=logfile -> appends "name rc secs" to results file
  local bin="$1" name="$2" log="$3"
  local start end rc
  start="$(date +%s)"
  if "$bin" >"$log" 2>&1; then rc=0; else rc=$?; fi
  end="$(date +%s)"
  echo "$name $rc $((end - start))" >> "$LOG_DIR/.results"
  return "$rc"
}

RESULTS="$LOG_DIR/.results"
: > "$RESULTS"

if [[ "$MODE" == "serial" ]]; then
  echo "==> running ${#BINARIES[@]} binaries, serially"
  for i in "${!BINARIES[@]}"; do
    echo "  [$((i + 1))/${#BINARIES[@]}] ${NAMES[$i]}"
    run_one "${BINARIES[$i]}" "${NAMES[$i]}" "$LOG_DIR/${NAMES[$i]}.log" || FAILURES+=("${NAMES[$i]}")
  done
else
  echo "==> running ${#BINARIES[@]} binaries, $JOBS at a time"
  for i in "${!BINARIES[@]}"; do
    while [[ "$(jobs -rp | wc -l | tr -d ' ')" -ge "$JOBS" ]]; do
      sleep 0.2
    done
    echo "  [start] ${NAMES[$i]}"
    run_one "${BINARIES[$i]}" "${NAMES[$i]}" "$LOG_DIR/${NAMES[$i]}.log" &
  done
  wait
fi

TOTAL_END="$(date +%s)"

# --- summary -----------------------------------------------------------------
SUMMARY="$LOG_DIR/summary.txt"
{
  echo "mode=$MODE jobs=$JOBS features=$FEATURES"
  echo "build_secs=$((BUILD_END - BUILD_START))"
  echo ""
  printf '%-32s %8s  %s\n' "SUITE" "SECONDS" "STATUS"
  sort -k1,1 "$RESULTS" | while read -r name rc secs; do
    [[ "$rc" -eq 0 ]] && status="ok" || status="FAIL"
    printf '%-32s %8d  %s\n' "$name" "$secs" "$status"
  done
  echo ""
  echo "total_wall_secs=$((TOTAL_END - TOTAL_START)) (tests only, build excluded)"
  # Derive failures from the results file (works for both serial and parallel,
  # where background subshells can't touch this shell's arrays).
  FAILED_LIST="$(awk '$2 != 0 { print $1 }' "$RESULTS" | tr '\n' ' ')"
  if [[ -n "${FAILED_LIST// /}" ]]; then
    echo "failed_suites: $FAILED_LIST"
  else
    echo "failed_suites: none"
  fi
} | tee "$SUMMARY"

echo "logs: $LOG_DIR"
[[ -z "${FAILED_LIST// /}" ]] || exit 1
