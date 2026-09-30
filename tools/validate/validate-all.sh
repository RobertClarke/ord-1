#!/usr/bin/env bash
# Runs the whole validation suite against a running fork:
#   1. oracle-check live mode (every row, every edge set; waits for equal heights),
#   2. oracle-check --baseline (recorded oracle answers, any height),
#   3. supply-crosscheck (every row, top-25 holder lists),
# while resource-watch samples the fork's process. Results land in --out.
#
#   set -a; source ~/.config/ord-sats/bitcoind.env; set +a   # ORD_BITCOIN_RPC_*, ORACLE_ORD_URL
#   tools/validate/validate-all.sh --ours=http://127.0.0.1:8090 [--oracle=URL] [--out=DIR]
#        [--pid=PID | --pgrep=PATTERN] [--data-dir=DIR] [--samples-per-row=20] [--seed=S]
#        [--baseline=tools/validate/baseline]
#
# Exit code: the worst of the three tools (0 ok, 1 mismatches, 2 incomplete).
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
ours=""
oracle="${ORACLE_ORD_URL:-}"
out=""
pid=""
pattern=""
data_dir=""
samples=20
seed="validate-all-$(date -u +%Y%m%d)"
baseline="$here/baseline"

for arg in "$@"; do
  case "$arg" in
    --ours=*) ours="${arg#*=}" ;;
    --oracle=*) oracle="${arg#*=}" ;;
    --out=*) out="${arg#*=}" ;;
    --pid=*) pid="${arg#*=}" ;;
    --pgrep=*) pattern="${arg#*=}" ;;
    --data-dir=*) data_dir="${arg#*=}" ;;
    --samples-per-row=*) samples="${arg#*=}" ;;
    --seed=*) seed="${arg#*=}" ;;
    --baseline=*) baseline="${arg#*=}" ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done
[ -n "$ours" ] && [ -n "$oracle" ] || { echo "--ours and --oracle (or ORACLE_ORD_URL) are required" >&2; exit 2; }
out="${out:-${TMPDIR:-/tmp}/ord-validate/run-$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$out"
echo "results in $out"

watch=""
if [ -n "$pid" ] || [ -n "$pattern" ]; then
  bash "$here/resource-watch.sh" ${pid:+--pid="$pid"} ${pattern:+--pgrep="$pattern"} ${data_dir:+--data-dir="$data_dir"} \
    --interval=10 --out="$out/resources.csv" > /dev/null 2> "$out/resources.err" &
  watch=$!
fi
cleanup() { [ -n "$watch" ] && kill "$watch" 2>/dev/null; true; }
trap cleanup EXIT

worst=0
record() { [ "$1" -gt "$worst" ] && worst=$1; true; }

curl -s -m 10 "$ours/status" > "$out/ours-status-before.json" || true

echo "== live oracle-check $(date -u +%T)"
node "$here/oracle-check.js" --ours="$ours" --oracle="$oracle" --samples-per-row="$samples" --seed="$seed" \
  --scan-concurrency=4 --out="$out/live.mismatches.jsonl" --summary-json="$out/live.summary.json" \
  --samples-out="$out/live.samples.jsonl" > "$out/live.txt" 2> "$out/live.log"
code=$?; record $code; echo "live: exit $code ($(tail -1 "$out/live.txt" 2>/dev/null))"

echo "== baseline oracle-check $(date -u +%T)"
node "$here/oracle-check.js" --ours="$ours" --baseline="$baseline" --oracle="$oracle" --scan-concurrency=4 \
  --out="$out/baseline.mismatches.jsonl" --summary-json="$out/baseline.summary.json" > "$out/baseline.txt" 2> "$out/baseline.log"
code=$?; record $code; echo "baseline: exit $code ($(tail -1 "$out/baseline.txt" 2>/dev/null))"

echo "== supply-crosscheck $(date -u +%T)"
node "$here/supply-crosscheck.js" --ours="$ours" --top=25 --out="$out/supply.diffs.jsonl" \
  --summary-json="$out/supply.summary.json" > "$out/supply.txt" 2> "$out/supply.log"
code=$?; record $code; echo "supply: exit $code ($(tail -1 "$out/supply.txt" 2>/dev/null))"

curl -s -m 10 "$ours/status" > "$out/ours-status-after.json" || true
if [ -n "$watch" ]; then
  kill "$watch" 2>/dev/null
  watch=""
  bash "$here/resource-watch.sh" --summary "$out/resources.csv" > "$out/resources.txt" 2>&1 && cat "$out/resources.txt"
fi
echo "== done $(date -u +%T), exit $worst"
exit "$worst"
