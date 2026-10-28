#!/usr/bin/env bash
# M2-001: build the hub Worker, apply the local D1 migrations, run it under
# `wrangler dev --local --test-scheduled` on a free port, wait for /healthz, then run one
# integration test (crates/hub/tests/<name>.rs) with HUB_URL set. wrangler is always stopped
# (trap; the whole process group, workerd included, is killed).
#
# Local only. Never runs `wrangler deploy`, `wrangler login` or anything `--remote`.
#   bash scripts/hub-test.sh smoke
set -euo pipefail

name=${1:?usage: scripts/hub-test.sh <test-name>, e.g. smoke}
[[ $name =~ ^[a-z0-9_-]+$ ]] || { echo "FAIL: bad test name: $name" >&2; exit 2; }

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
HUB_DIR=$REPO/crates/hub
[[ -f $HUB_DIR/tests/$name.rs ]] || { echo "FAIL: no such test: crates/hub/tests/$name.rs" >&2; exit 2; }

# RUSTFLAGS (lld) is for the native target only. `strip` breaks worker-build 0.8.7
# (docs/spikes/cloudflare.md caveat 2), so it stays off.
unset RUSTFLAGS RUSTC_WRAPPER
export CARGO_PROFILE_RELEASE_STRIP=false
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export WRANGLER_SEND_METRICS=false

fail() { echo "FAIL: $*" >&2; exit 1; }

STATE=$(mktemp -d)
LOG=$STATE/wrangler.log
WRANGLER_PID=""
# wrangler's bundling scratch (crates/hub/.wrangler) is removed on exit when this run created it.
scratch_created=0
[[ -e $HUB_DIR/.wrangler ]] || scratch_created=1

stop_wrangler() {
  local pid=$1
  [[ -n $pid ]] || return 0
  kill -TERM -- "-$pid" 2>/dev/null || true
  # Give the group 5 s to exit, then SIGKILL it. The watchdog is a subshell, not part of the group.
  ( sleep 5; kill -KILL -- "-$pid" 2>/dev/null || true ) &
  local dog=$!
  wait "$pid" 2>/dev/null || true
  kill "$dog" 2>/dev/null || true
  wait "$dog" 2>/dev/null || true
  kill -KILL -- "-$pid" 2>/dev/null || true
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  stop_wrangler "$WRANGLER_PID"
  if (( status != 0 )) && [[ -f $LOG ]]; then
    echo "---- wrangler log (tail) ----" >&2
    tail -n 60 "$LOG" >&2 || true
  fi
  if (( scratch_created )); then rm -rf "$HUB_DIR/.wrangler"; fi
  rm -rf "$STATE"
  exit "$status"
}
trap cleanup EXIT INT TERM

echo "== worker-build --release ($HUB_DIR)"
start=$(date +%s)
(cd "$HUB_DIR" && worker-build --release) >"$STATE/build.log" 2>&1 \
  || { tail -n 40 "$STATE/build.log" >&2; fail "worker-build --release"; }
echo "built in $(( $(date +%s) - start ))s"

echo "== d1 migrations (local)"
(cd "$HUB_DIR" && wrangler d1 migrations apply DB --local --persist-to "$STATE/persist") \
  >"$STATE/d1.log" 2>&1 \
  || { cat "$STATE/d1.log" >&2; fail "d1 migrations apply"; }

# M2-003a: a random per-run bootstrap secret (never a real one). The hub gets it as a --var and the
# test process as an env var, so tests/authz.rs can call POST /v1/auth/bootstrap.
BOOTSTRAP_OWNER_TOKEN=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
export BOOTSTRAP_OWNER_TOKEN
# M2-009: a random per-run attachment URL signing key (the hub fails closed without one).
ATTACHMENT_SIGNING_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')

PORT=$(node -e 'const s=require("net").createServer().listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})')
BASE="http://127.0.0.1:$PORT"
echo "== wrangler dev --local --test-scheduled on port $PORT"
(cd "$HUB_DIR" && exec setsid wrangler dev --local --test-scheduled --ip 127.0.0.1 --port "$PORT" \
  --persist-to "$STATE/persist" --var "BOOTSTRAP_OWNER_TOKEN:$BOOTSTRAP_OWNER_TOKEN" \
  --var "ATTACHMENT_SIGNING_KEY:$ATTACHMENT_SIGNING_KEY" \
  --show-interactive-dev-session=false) >"$LOG" 2>&1 </dev/null &
WRANGLER_PID=$!

# wrangler dev reruns the [build] command (warm, a few seconds). Ready = /healthz answers 200.
ready=0
for i in $(seq 1 240); do
  if ! jobs -rp | grep -qx "$WRANGLER_PID"; then
    fail "wrangler dev exited early"
  fi
  code=$(curl -s --max-time 2 -o /dev/null -w '%{http_code}' "$BASE/healthz" || true)
  if [[ $code == 200 ]]; then
    echo "ready after ~${i}s"
    ready=1
    break
  fi
  sleep 1
done
(( ready )) || fail "wrangler dev not ready after 240s (GET /healthz != 200)"

echo "== cargo test -p hub --features live --test $name (HUB_URL=$BASE)"
(cd "$REPO" && HUB_URL="$BASE" cargo test -p hub --features live --test "$name" -- --nocapture)
echo "PASS hub-test $name"
