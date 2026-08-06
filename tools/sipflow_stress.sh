#!/bin/bash
# sipflow_stress.sh — controlled stress test for the sipflow collector.
#
# Starts a fresh sipflow instance (production-matching params), runs the
# UDP load generator at a fixed pps for a duration, and records:
#   throughput (rows flushed/s), loss (sent vs written), queue depths,
#   kernel UDP drops (RcvbufErrors delta), RSS trend.
#
# Usage:
#   tools/sipflow_stress.sh --pps 50000 --duration 300 --label baseline [--port 9000]
#
# Results go to /tmp/sipflow_stress_<label>.{log,rss,metrics,snmp}
set -u

LABEL=""
PPS=20000
DURATION=120
PORT=9000
HTTP_PORT=3001
ROOT=/tmp/sipflow-stress
BIN=${BIN:-/home/rzl/workspace/rs/rustpbx/target/release/sipflow}
LOADGEN=${LOADGEN:-/home/rzl/workspace/rs/rustpbx/target/release/examples/sipflow_loadgen}

while [ $# -gt 0 ]; do
    case "$1" in
        --pps) PPS=$2; shift 2 ;;
        --duration) DURATION=$2; shift 2 ;;
        --label) LABEL=$2; shift 2 ;;
        --port) PORT=$2; shift 2 ;;
        --http-port) HTTP_PORT=$2; shift 2 ;;
        --root) ROOT=$2; shift 2 ;;
        *) echo "unknown: $1"; exit 1 ;;
    esac
done
[ -n "$LABEL" ] || { echo "--label required"; exit 1; }

BASE=/tmp/sipflow_stress_$LABEL
rm -f "$BASE.log" "$BASE.rss" "$BASE.metrics" "$BASE.snmp"
: > "$BASE.log"

SIPFLOW_LOG="$BASE.log"
SIPFLOW_PID=""
SAMPLER_PID=""

cleanup() {
    [ -n "$SAMPLER_PID" ] && kill "$SAMPLER_PID" 2>/dev/null
    [ -n "$SIPFLOW_PID" ] && kill "$SIPFLOW_PID" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT

echo "== sipflow stress: pps=$PPS duration=${DURATION}s label=$LABEL =="
echo "root=$ROOT  bin=$BIN"

# Fresh data dir per run
rm -rf "$ROOT"
mkdir -p "$ROOT"

"$BIN" \
    --port "$PORT" --http-port "$HTTP_PORT" --root "$ROOT" \
    --engine sqlite --shards 4 \
    --buffer-size 250000 --recv-buffer-size 33554432 \
    --flush-count 5000 --flush-interval 1 \
    --id-cache-size 65536 --compress-level 1 \
    --subdirs daily --log-file "$SIPFLOW_LOG" --log-level info \
    >> "$BASE.log" 2>&1 &
SIPFLOW_PID=$!

# Wait for readiness
for i in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done
if ! kill -0 "$SIPFLOW_PID" 2>/dev/null; then
    echo "FATAL: sipflow failed to start"; tail -20 "$BASE.log"; exit 1
fi
echo "sipflow up (pid=$SIPFLOW_PID, http=$HTTP_PORT)"

snmp_now() {
    grep -A1 "^Udp: " /proc/net/snmp | grep -v "^UdpLite" | tail -1
}
snmp_snapshot() {
    snmp_now
}

# Start/end kernel UDP snapshots
S0=$(snmp_snapshot)

# Sampler loop (every 5s): metrics + RSS
(
    echo "t_secs rows_rtp rows_sip backpressure worker_q flusher_q pending rss_kb"
    start=$(date +%s)
    while kill -0 "$SIPFLOW_PID" 2>/dev/null; do
        t=$(( $(date +%s) - start ))
        m=$(curl -sf "http://127.0.0.1:$HTTP_PORT/metrics" 2>/dev/null || true)
        rr=$(echo "$m" | grep '^sipflow_flush_rows_total{component="sipflow",type="rtp"}' | awk '{print $2}')
        rs=$(echo "$m" | grep '^sipflow_flush_rows_total{component="sipflow",type="sip"}' | awk '{print $2}')
        bp=$(echo "$m" | grep '^sipflow_record_backpressure_dropped_total{component="sipflow"}' | awk '{print $2}')
        wq=$(echo "$m" | grep '^sipflow_worker_queue_depth{component="sipflow"}' | awk '{print $2}')
        fq=$(echo "$m" | grep '^sipflow_flusher_queue_depth{component="sipflow"}' | awk '{print $2}')
        pd=$(echo "$m" | grep '^sipflow_pending_items{component="sipflow"}' | awk '{print $2}')
        rss=$(grep VmRSS /proc/$SIPFLOW_PID/status 2>/dev/null | awk '{print $2}')
        echo "$t ${rr:-0} ${rs:-0} ${bp:-0} ${wq:-0} ${fq:-0} ${pd:-0} ${rss:-0}"
        sleep 5
    done
) >> "$BASE.metrics" &
SAMPLER_PID=$!

echo "running loadgen pps=$PPS duration=${DURATION}s ..."
"$LOADGEN" --target "127.0.0.1:$PORT" --pps "$PPS" --duration "$DURATION" --calls 2048 2>&1 | tee "$BASE.loadgen"

# Drain tail so flush counters settle
sleep 8

S1=$(snmp_snapshot)

kill "$SAMPLER_PID" 2>/dev/null; wait "$SAMPLER_PID" 2>/dev/null
SAMPLER_PID=""

echo "$S0" > "$BASE.snmp"
echo "$S1" >> "$BASE.snmp"

# ---- Summary ----
FIRST=$(head -2 "$BASE.metrics" | tail -1)
LAST=$(tail -1 "$BASE.metrics")
f_r0=$(echo "$FIRST" | awk '{print $2}'); f_s0=$(echo "$FIRST" | awk '{print $3}'); f_b0=$(echo "$FIRST" | awk '{print $4}')
l_r1=$(echo "$LAST"  | awk '{print $2}'); l_s1=$(echo "$LAST"  | awk '{print $3}'); l_b1=$(echo "$LAST"  | awk '{print $4}')
rows_rtp=$((l_r1 - f_r0)); rows_sip=$((l_s1 - f_s0)); rows=$((rows_rtp + rows_sip))
bp_delta=$((l_b1 - f_b0))
t_span=$(echo "$LAST" | awk '{print $1}')
[ "$t_span" -lt 1 ] && t_span=1
rate=$((rows / t_span))

sent=$(grep "loadgen done" "$BASE.loadgen" 2>/dev/null | grep -oP 'sent=\K[0-9]+' || echo 0)
sent=${sent:-0}
loss=$((sent - rows)); [ $loss -lt 0 ] && loss=0
loss_pct=$(awk "BEGIN {if ($sent>0) printf \"%.1f\", 100*$loss/$sent; else printf \"0\"}")

in0=$(echo "$S0" | awk '{print $2}'); rcv0=$(echo "$S0" | awk '{print $6}')
in1=$(echo "$S1" | awk '{print $2}'); rcv1=$(echo "$S1" | awk '{print $6}')
in_d=$((in1 - in0)); rcv_d=$((rcv1 - rcv0))
in_d=${in_d:-0}; rcv_d=${rcv_d:-0}

rss_first=$(head -2 "$BASE.metrics" | tail -1 | awk '{print $8}')
rss_last=$(tail -1 "$BASE.metrics" | awk '{print $8}')
rss_max=$(awk 'NR>1 {v=$8+0; if(v>max)max=v} END{print max+0}' "$BASE.metrics")
rss_first=${rss_first:-0}; rss_last=${rss_last:-0}; rss_max=${rss_max:-0}
rss_delta=$((rss_last - rss_first))

echo ""
echo "=========================================================="
echo "  SUMMARY ($LABEL)  pps=$PPS duration=${DURATION}s"
echo "=========================================================="
echo "  sent               : $sent"
echo "  rows written       : $rows (rtp=$rows_rtp sip=$rows_sip)"
echo "  loss               : $loss ($loss_pct%)"
echo "  write rate         : ~$rate rows/s (over ${t_span}s)"
echo "  backpressure drops : $bp_delta"
echo "  kernel InDatagrams Δ: $in_d   RcvbufErrors Δ: $rcv_d"
echo "  RSS first/last/max  : $((rss_first/1024))MB / $((rss_last/1024))MB / $((rss_max/1024))MB  (Δ=$((rss_delta/1024))MB)"
echo "=========================================================="
echo "log: $BASE.log  metrics: $BASE.metrics  snmp: $BASE.snmp  rss: sampled in $BASE.metrics"
