#!/usr/bin/env bash
set -euo pipefail

echo "================================"
echo "  snyd External Benchmarks"
echo "================================"

CORPUS=/tmp/snyd_benchmark_corpus
rm -rf "$CORPUS"
mkdir -p "$CORPUS"

echo "Generating 100,000 test files..."
for i in $(seq 0 99999); do
    dir="$CORPUS/dir$((i % 1000))"
    mkdir -p "$dir"
    echo "content $i" > "$dir/file_${i}_budget_report_2024.txt"
done
echo "Done: 100,000 files"

BIN=/Users/wica/snyd-community/target/release/snyd

echo ""
echo "Starting snyd..."
rm -f /tmp/snyd-bench.sock
"$BIN" -s /tmp/snyd-bench.sock -d "$CORPUS" -c /tmp/snyd-bench-cache --log-level warn &
DAEMON_PID=$!
sleep 4

# Warmup
echo '{"id":"w","query":"budget","max_results":10,"scopes":[]}' | nc -U /tmp/snyd-bench.sock > /dev/null 2>&1 || true

bench_snyd() {
    local query=$1
    local label=$2
    local t0 t1
    t0=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    echo '{"id":"b","query":"'"$query"'","max_results":20,"scopes":[]}' | nc -U /tmp/snyd-bench.sock > /dev/null 2>&1 || true
    t1=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    local ms
    ms=$(perl -e "printf \"%.2f\", ($t1 - $t0) * 1000")
    echo "snyd,$label,${ms}ms"
}

bench_find() {
    local query=$1
    local label=$2
    local t0 t1
    t0=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    find "$CORPUS" -name "*$query*" 2>/dev/null | head -20 > /dev/null || true
    t1=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    local ms
    ms=$(perl -e "printf \"%.2f\", ($t1 - $t0) * 1000")
    echo "find,$label,${ms}ms"
}

bench_mdfind() {
    local query=$1
    local label=$2
    local t0 t1
    t0=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    mdfind -onlyin "$CORPUS" "$query" 2>/dev/null | head -20 > /dev/null || true
    t1=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
    local ms
    ms=$(perl -e "printf \"%.2f\", ($t1 - $t0) * 1000")
    echo "mdfind,$label,${ms}ms"
}

echo ""
echo "Running searches..."
for q in budget bdgt file_50000; do
    bench_snyd "$q" "$q"
    bench_find "$q" "$q"
    bench_mdfind "$q" "$q"
done

kill $DAEMON_PID 2>/dev/null || true
rm -f /tmp/snyd-bench.sock
rm -rf "$CORPUS"
echo ""
echo "Benchmark complete."
