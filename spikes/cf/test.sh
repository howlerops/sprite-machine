#!/usr/bin/env bash
# M0-017: build the spike Worker, run it under `wrangler dev --local`, exercise every route.
# Local only. Never runs `wrangler deploy`, `wrangler login` or anything `--remote`.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"
SPIKE_DIR=$PWD

# RUSTFLAGS from docs/dev.md (-fuse-ld=lld) is for the native target only.
unset RUSTFLAGS RUSTC_WRAPPER
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$SPIKE_DIR/target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export WRANGLER_SEND_METRICS=false

fail() { echo "FAIL: $*" >&2; exit 1; }

# --- 1. no tokio runtime -----------------------------------------------------------------
# worker 0.8.7 depends on tokio unconditionally (default-features = false, used only for the
# AsyncRead/AsyncWrite traits on its TCP Socket). We assert that tokio is reached only through
# `worker` and has no features enabled (so no rt, net, time, sync or macros).
echo "== dependency check"
tokio_lines=$(cargo tree --target wasm32-unknown-unknown -e normal -f '{p} [{f}]' --prefix none \
  | grep -E '^tokio ' | sort -u || true)
echo "tokio in tree: ${tokio_lines:-<none>}"
if [[ -n "$tokio_lines" ]]; then
  [[ "$tokio_lines" =~ ^tokio\ v[0-9.]+\ \[\]$ ]] || fail "tokio has features enabled: $tokio_lines"
  dependents=$(cargo tree --target wasm32-unknown-unknown -e normal -i tokio --depth 1 --prefix none \
    | sed 1d | sort -u)
  [[ "$dependents" =~ ^worker\ v ]] || fail "tokio pulled in by something other than worker: $dependents"
fi

# --- 2. build ----------------------------------------------------------------------------
echo "== worker-build --release"
start=$(date +%s)
worker-build --release
echo "build time: $(( $(date +%s) - start ))s"
for f in build/index.js build/index_bg.wasm; do
  [[ -s $f ]] || fail "missing build output $f"
done
raw=$(cat build/index.js build/index_bg.wasm | wc -c)
gz=$(cat build/index.js build/index_bg.wasm | gzip -9 | wc -c)
echo "bundle: raw ${raw} bytes, gzip ${gz} bytes (index.js + index_bg.wasm)"
# Platform limit is 64 MiB uncompressed (docs/spikes/cloudflare.md). This is our own budget,
# kept small for the 1 s startup limit and cold starts.
(( gz < 1024 * 1024 )) || fail "gzip bundle ${gz} bytes exceeds the 1 MiB spike budget"

# --- 3. wrangler dev --local -------------------------------------------------------------
STATE=$(mktemp -d)
LOG="$STATE/wrangler.log"
WRANGLER_PID=""
cleanup() {
  status=$?
  if [[ -n "$WRANGLER_PID" ]]; then
    # wrangler runs in its own process group (setsid); kill the whole group (workerd too).
    kill -TERM -- "-$WRANGLER_PID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 -- "-$WRANGLER_PID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL -- "-$WRANGLER_PID" 2>/dev/null || true
  fi
  if (( status != 0 )) && [[ -f $LOG ]]; then
    echo "---- wrangler log (tail) ----" >&2
    tail -n 60 "$LOG" >&2 || true
  fi
  rm -rf "$STATE"
  exit "$status"
}
trap cleanup EXIT INT TERM

echo "== d1 migrations (local)"
wrangler d1 migrations apply DB --local --persist-to "$STATE/persist" >"$STATE/d1.log" 2>&1 \
  || { cat "$STATE/d1.log" >&2; fail "d1 migrations apply"; }

PORT=$(node -e 'const s=require("net").createServer().listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})')
BASE="http://127.0.0.1:$PORT"
echo "== wrangler dev --local on port $PORT"
setsid wrangler dev --local --ip 127.0.0.1 --port "$PORT" --persist-to "$STATE/persist" \
  --show-interactive-dev-session=false >"$LOG" 2>&1 < /dev/null &
WRANGLER_PID=$!

# wrangler dev reruns the [build] command (cargo is a no-op; wasm-bindgen + wasm-opt rerun).
for i in $(seq 1 240); do
  kill -0 "$WRANGLER_PID" 2>/dev/null || fail "wrangler dev exited early"
  if [[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health" || true)" == 200 ]]; then
    echo "ready after ~${i}s"
    break
  fi
  (( i < 240 )) || fail "wrangler dev not ready after 240s"
  sleep 1
done

# --- 4. routes ---------------------------------------------------------------------------
# req METHOD PATH [BODY] -> prints "<status> <body>"
req() {
  local method=$1 path=$2 body=${3-}
  local out
  if [[ -n $body ]]; then
    out=$(curl -sS -X "$method" --data-binary "$body" -w $'\n%{http_code}' "$BASE$path")
  else
    out=$(curl -sS -X "$method" -w $'\n%{http_code}' "$BASE$path")
  fi
  printf '%s %s' "${out##*$'\n'}" "${out%$'\n'*}"
}
expect() {
  local want=$1; shift
  local got
  got=$(req "$@")
  [[ "$got" == "$want" ]] || fail "$* -> want [$want], got [$got]"
  echo "ok: $* -> $got"
}

echo "== HTTP routes"
expect "200 ok" GET /health
expect "404 " GET /nope

# Durable Object (SQLite): counter persists across requests, one database per name.
expect "200 0" GET /counter/room-a
expect "200 1" POST /counter/room-a
expect "200 2" POST /counter/room-a
expect "200 2" GET /counter/room-a
expect "200 1" POST /counter/room-b
expect "200 2" GET /counter/room-a
expect "426 expected Upgrade: websocket" GET /ws/room-a

# D1
expect '200 []' GET /d1/notes
expect '200 {"id":1,"body":"hello d1"}' POST /d1/notes "hello d1"
expect '200 {"id":2,"body":"second"}' POST /d1/notes "second"
expect '200 [{"id":1,"body":"hello d1"},{"id":2,"body":"second"}]' GET /d1/notes

# R2
expect "201 " PUT /r2/greeting "hello r2"
expect "200 hello r2" GET /r2/greeting
expect "404 " GET /r2/missing

echo "== WebSocket (hibernatable, echo + auto-response)"
node ws-check.mjs "ws://127.0.0.1:$PORT"

# The DO's SQLite state is untouched by the WebSocket traffic.
expect "200 2" GET /counter/room-a

echo "ALL PASSED"
