#!/usr/bin/env bash
# M0-008b: the hub's WORKSPACE binding to the Workspace DO, end to end, locally.
#
# Builds the hub with its [build] command (crates/hub/wrangler.toml), then runs both Workers in one
# `wrangler dev --enable-containers=false` (the hub with the WORKSPACE binding, and the workspace
# Worker from workers/workspace with FakeSandbox). It drives start -> status -> stop (and two
# error cases) through the hub's DEV_FIXTURES route, POST /v1/dev/workspaces/<id>/<method>, and
# checks each answer. wrangler is always stopped (trap: the whole process group, workerd included).
#
# Local only. Never runs `wrangler deploy`, `wrangler login` or anything `--remote`.
#   bash scripts/workspace-dev-test.sh
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
HUB_DIR=$REPO/crates/hub
WS_DIR=$REPO/workers/workspace
WRANGLER=$WS_DIR/node_modules/.bin/wrangler

# The wasm build must not see the native lld flags (same as hub-test.sh and the cli).
unset RUSTFLAGS RUSTC_WRAPPER
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export WRANGLER_SEND_METRICS=false

fail() { echo "FAIL: $*" >&2; exit 1; }

STATE=$(mktemp -d)
LOG=$STATE/wrangler.log
WRANGLER_PID=""
# wrangler's bundling scratch is removed on exit when this run created it (the dirs are gitignored).
scratch_hub=0; [[ -e $HUB_DIR/.wrangler ]] || scratch_hub=1
scratch_ws=0; [[ -e $WS_DIR/.wrangler ]] || scratch_ws=1

stop_wrangler() {
  local pid=$1
  [[ -n $pid ]] || return 0
  kill -TERM -- "-$pid" 2>/dev/null || true
  # Give the group 10 s to exit, then SIGKILL it. The watchdog is a subshell outside the group.
  ( sleep 10; kill -KILL -- "-$pid" 2>/dev/null || true ) &
  local dog=$!
  wait "$pid" 2>/dev/null || true
  kill "$dog" 2>/dev/null || true
  wait "$dog" 2>/dev/null || true
  # Wait until no process of that group remains (workerd is a child of wrangler, same group).
  local i
  for i in $(seq 1 100); do
    pgrep -g "$pid" >/dev/null 2>&1 || return 0
    sleep 0.1
  done
  kill -KILL -- "-$pid" 2>/dev/null || true
  for i in $(seq 1 50); do
    pgrep -g "$pid" >/dev/null 2>&1 || return 0
    sleep 0.1
  done
  echo "FAIL: processes left in group $pid after stop" >&2
  return 1
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  stop_wrangler "$WRANGLER_PID" || status=1
  if (( status != 0 )) && [[ -f $LOG ]]; then
    echo "---- wrangler log (tail) ----" >&2
    tail -n 60 "$LOG" >&2 || true
  fi
  if (( scratch_hub )); then rm -rf "$HUB_DIR/.wrangler"; fi
  if (( scratch_ws )); then rm -rf "$WS_DIR/.wrangler"; fi
  rm -rf "$STATE"
  exit "$status"
}
trap cleanup EXIT INT TERM

if [[ ! -d $WS_DIR/node_modules ]]; then
  echo "== npm ci (workers/workspace)"
  (cd "$WS_DIR" && npm ci) >"$STATE/npm.log" 2>&1 \
    || { tail -n 40 "$STATE/npm.log" >&2; fail "npm ci in workers/workspace"; }
fi
[[ -x $WRANGLER ]] || fail "no wrangler at $WRANGLER (npm ci failed?)"

echo "== worker-build --profile wasm-release (crates/hub [build])"
start=$(date +%s)
(cd "$HUB_DIR" && worker-build --profile wasm-release) >"$STATE/build.log" 2>&1 \
  || { tail -n 40 "$STATE/build.log" >&2; fail "worker-build"; }
echo "built in $(( $(date +%s) - start ))s"

PORT=$(node -e 'const s=require("net").createServer().listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})')
BASE="http://127.0.0.1:$PORT"
echo "== wrangler dev (hub + workspace, containers off, FakeSandbox, DEV_FIXTURES) on port $PORT"
(cd "$HUB_DIR" && exec setsid "$WRANGLER" dev \
  -c "$HUB_DIR/wrangler.toml" -c "$WS_DIR/wrangler.dev.jsonc" \
  --local --ip 127.0.0.1 --port "$PORT" --persist-to "$STATE/persist" \
  --enable-containers=false \
  --var DEV_FAKE_SANDBOX:1 --var DEV_FIXTURES:1 \
  --show-interactive-dev-session=false) >"$LOG" 2>&1 </dev/null &
WRANGLER_PID=$!

# Ready = the hub answers the fixture route for the Workspace DO (status) with 200.
ready=0
probe_id="ready-$$"
for i in $(seq 1 240); do
  if ! kill -0 "$WRANGLER_PID" 2>/dev/null; then
    fail "wrangler dev exited early"
  fi
  code=$(curl -s --max-time 5 -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' \
    --data '{}' "$BASE/v1/dev/workspaces/$probe_id/status" || true)
  if [[ $code == 200 ]]; then
    echo "ready after ~${i}s"
    ready=1
    break
  fi
  sleep 1
done
(( ready )) || fail "wrangler dev not ready after 240s (fixture route != 200)"

WS="wsdev-$$"
RPC_CODE=""
RPC_BODY=""

# rpc <method> <json>: POST the fixture route; sets RPC_CODE (HTTP status) and RPC_BODY.
rpc() {
  local out
  out=$(curl -sS -X POST -H 'content-type: application/json' --data "$2" \
    -w '\n%{http_code}' "$BASE/v1/dev/workspaces/$WS/$1")
  RPC_CODE=${out##*$'\n'}
  RPC_BODY=${out%$'\n'*}
}

# expect <label> <http-status> <substring>: checks the last rpc answer.
expect() {
  if [[ $RPC_CODE != "$2" || $RPC_BODY != *"$3"* ]]; then
    echo "FAIL $1: want HTTP $2 with $3, got HTTP $RPC_CODE: $RPC_BODY" >&2
    exit 1
  fi
  echo "ok   $1: HTTP $RPC_CODE $RPC_BODY"
}

IMG="registry.example/workspace:1"

rpc status '{}'
expect status-initial 200 '"state":"stopped"'

rpc start "{\"image\":\"$IMG\",\"instance\":\"standard-2\"}"
expect start 200 '"state":"running"'

rpc status '{}'
expect status-running 200 '"state":"running"'

rpc start "{\"image\":\"$IMG\"}"
expect start-twice 409 '"code":"invalid_state"'

rpc exec '{"cmd":["echo","hi"]}'
expect exec-running 200 '"stdout":"hi\n"'

rpc stop '{}'
expect stop 200 '"wasRunning":true'

rpc status '{}'
expect status-stopped 200 '"state":"stopped"'

rpc exec '{"cmd":["echo","hi"]}'
expect exec-stopped 409 '"code":"invalid_state"'

rpc nosuchmethod '{}'
expect unknown-method 404 '"error":"not_found"'

echo "workspace-dev-test: OK"
