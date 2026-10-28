#!/usr/bin/env bash
# Smoke test: wrangler dev (containers disabled, FakeSandbox) + curl against /rpc/start, /status, /stop.
# Real container start/stop and backup/restore are verified on the deployed preview env instead.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

log="$(mktemp)"
pid=""

cleanup() {
  local rc=$?
  if [[ -n "$pid" ]]; then
    # wrangler runs in its own session/process group (setsid): kill the whole group.
    kill -TERM -- "-$pid" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.3
    done
    kill -KILL -- "-$pid" 2>/dev/null || true
  fi
  if [[ $rc -ne 0 ]]; then
    echo "--- wrangler log ---" >&2
    tail -n 40 "$log" >&2 || true
  fi
  rm -f "$log"
  exit $rc
}
trap cleanup EXIT INT TERM

port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
base="http://127.0.0.1:${port}/w/smoke-$$"

setsid npx wrangler dev -c wrangler.dev.jsonc --local --ip 127.0.0.1 --port "$port" \
  --enable-containers=false --var DEV_FAKE_SANDBOX:1 >"$log" 2>&1 &
pid=$!

for _ in $(seq 1 120); do
  if curl -fsS -o /dev/null -X POST "$base/rpc/status" 2>/dev/null; then break; fi
  kill -0 "$pid" 2>/dev/null || { echo "wrangler exited early" >&2; exit 1; }
  sleep 0.5
done

rpc() { curl -fsS -X POST -H 'content-type: application/json' --data "$2" "$base/rpc/$1"; }

expect() { # expect <label> <json> <substring>
  if [[ "$2" != *"$3"* ]]; then
    echo "FAIL $1: expected $3 in: $2" >&2
    exit 1
  fi
  echo "ok   $1: $2"
}

expect status-initial "$(rpc status '{}')" '"state":"stopped"'
expect start "$(rpc start '{"image":"registry.example/workspace:1","instance":"standard-2"}')" '"state":"running"'
expect status-running "$(rpc status '{}')" '"state":"running"'
expect stop "$(rpc stop '{}')" '"wasRunning":true'
expect status-stopped "$(rpc status '{}')" '"state":"stopped"'

echo "dev-smoke: OK"
