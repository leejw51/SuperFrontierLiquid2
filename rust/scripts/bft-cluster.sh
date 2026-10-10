#!/usr/bin/env bash
# A local BFT validator cluster: N `cb-node validator` processes on one
# machine, each with its own data directory, RPC, dashboard and p2p port,
# all seeded to each other.
#
#   scripts/bft-cluster.sh start [N] [-- extra node args for validator 0]
#   scripts/bft-cluster.sh stop
#   scripts/bft-cluster.sh kill I        # stop validator I only
#   scripts/bft-cluster.sh restart I     # start validator I again on its data
#   scripts/bft-cluster.sh status
#   scripts/bft-cluster.sh logs [I]
#
# Ports for validator I: RPC 7700+I, dashboard 18080+I, p2p 7800+I. The
# gateway, oracle and web app talk to validator 0 on 7700 exactly as they
# talk to the single-node sequencer.
#
# Environment: GENESIS (default genesis-full.jsonl), BFT_DATA (default
# ./data-bft), ROUND_TIMEOUT_MS (500), IDLE_INTERVAL_MS (100),
# MAX_BLOCK_TXS (10000), KEEP_BLOCKS (20000), RUST_LOG (info).
#
# KEEP_BLOCKS is how far back a validator that was down can catch up from
# the others: a rejoining validator syncs the executed blocks it missed from
# its peers, and a peer only has the bodies it kept.
set -euo pipefail
cd "$(dirname "$0")/.."

NODE_BIN=${NODE_BIN:-target/release/cb-node}
GENESIS=${GENESIS:-genesis-full.jsonl}
BFT_DATA=${BFT_DATA:-./data-bft}
ROUND_TIMEOUT_MS=${ROUND_TIMEOUT_MS:-500}
IDLE_INTERVAL_MS=${IDLE_INTERVAL_MS:-100}
MAX_BLOCK_TXS=${MAX_BLOCK_TXS:-10000}
KEEP_BLOCKS=${KEEP_BLOCKS:-20000}
export RUST_LOG=${RUST_LOG:-info}
# Threads per validator for signature recovery and the parallel work on a
# proposal. Every validator on this machine shares its cores, and by default
# each would size both pools to all of them: four validators on eighteen
# cores ran 144 rayon threads, most of them spinning for work. The budget is
# the cores split between the validators, at least two each.
cores=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)

count_file="$BFT_DATA/count"

seeds_for() {
  local me=$1 n=$2 out=""
  for ((j = 0; j < n; j++)); do
    [ "$j" = "$me" ] && continue
    out="$out --p2p-seed 127.0.0.1:$((7800 + j))"
  done
  echo "$out"
}

launch() {
  local i=$1 n=$2; shift 2
  local dir="$BFT_DATA/node$i"
  local log="$BFT_DATA/node$i.log"
  local pidf="$BFT_DATA/node$i.pid"
  if [ -f "$pidf" ] && kill -0 "$(cat "$pidf")" 2>/dev/null; then
    echo "validator $i already running (pid $(cat "$pidf"))"
    return
  fi
  mkdir -p "$dir"
  if [ ! -f "$dir/CURRENT" ]; then
    "$NODE_BIN" --data-dir "$dir" init --genesis "$GENESIS" --consensus bft >>"$log" 2>&1
  fi
  # shellcheck disable=SC2046
  local threads=${VERIFY_THREADS:-$(( (cores + n - 1) / n ))}
  [ "$threads" -lt 2 ] && threads=2
  RAYON_NUM_THREADS="$threads" \
  nohup "$NODE_BIN" --data-dir "$dir" validator \
    --listen 127.0.0.1:$((7700 + i)) \
    --verify-threads "$threads" \
    --dashboard-listen 127.0.0.1:$((18080 + i)) \
    --dev-key-seed "cb-validator-$i" \
    --p2p-listen 127.0.0.1:$((7800 + i)) \
    $(seeds_for "$i" "$n") \
    --round-timeout-ms "$ROUND_TIMEOUT_MS" \
    --idle-interval-ms "$IDLE_INTERVAL_MS" \
    --max-block-txs "$MAX_BLOCK_TXS" \
    --keep-blocks "$KEEP_BLOCKS" \
    "$@" >>"$log" 2>&1 &
  local pid=$!
  sleep 1
  if kill -0 "$pid" 2>/dev/null; then
    echo "$pid" >"$pidf"
    echo "validator $i started (pid $pid)  rpc 127.0.0.1:$((7700 + i))  dashboard http://127.0.0.1:$((18080 + i))  log $log"
  else
    echo "validator $i exited at once; the end of $log says why:"
    tail -5 "$log" | sed 's/^/    /'
    return 1
  fi
}

cmd=${1:-status}; shift || true
case "$cmd" in
  start)
    n=${1:-4}
    if [ "${1:-}" != "" ] && [ "${1:-}" != "--" ]; then shift; fi
    [ "${1:-}" = "--" ] && shift
    [ -x "$NODE_BIN" ] || { echo "build first: cargo build --release -p cb-node"; exit 1; }
    mkdir -p "$BFT_DATA"
    echo "$n" >"$count_file"
    for ((i = 0; i < n; i++)); do
      if [ "$i" = 0 ]; then launch "$i" "$n" "$@"; else launch "$i" "$n"; fi
    done
    ;;
  stop)
    n=$(cat "$count_file" 2>/dev/null || echo 4)
    for ((i = 0; i < n; i++)); do
      pidf="$BFT_DATA/node$i.pid"
      if [ -f "$pidf" ]; then
        pid=$(cat "$pidf")
        kill "$pid" 2>/dev/null && echo "stopped validator $i (pid $pid)" || true
        rm -f "$pidf"
      fi
    done
    pkill -f "$NODE_BIN --data-dir $BFT_DATA" 2>/dev/null || true
    ;;
  kill)
    i=$1; pidf="$BFT_DATA/node$i.pid"
    if [ -f "$pidf" ]; then kill "$(cat "$pidf")" 2>/dev/null || true; rm -f "$pidf"; echo "killed validator $i"; fi
    ;;
  restart)
    i=$1; n=$(cat "$count_file")
    launch "$i" "$n"
    ;;
  status)
    n=$(cat "$count_file" 2>/dev/null || echo 4)
    for ((i = 0; i < n; i++)); do
      pidf="$BFT_DATA/node$i.pid"
      if [ -f "$pidf" ] && kill -0 "$(cat "$pidf")" 2>/dev/null; then
        head=$(curl -s "http://127.0.0.1:$((18080 + i))/api/status" 2>/dev/null | head -c 300 || true)
        echo "validator $i: up (pid $(cat "$pidf"))  $head"
      else
        echo "validator $i: down"
      fi
    done
    ;;
  logs)
    i=${1:-0}
    tail -f "$BFT_DATA/node$i.log"
    ;;
  *)
    echo "usage: $0 start [N] [-- node0 args] | stop | kill I | restart I | status | logs [I]"; exit 1
    ;;
esac
