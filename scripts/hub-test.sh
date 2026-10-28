#!/usr/bin/env bash
# M2-001: build the hub Worker, apply the local D1 migrations, run it under
# `wrangler dev --local --test-scheduled` on a free port, wait for /healthz, then run one
# integration test (crates/hub/tests/<name>.rs) with HUB_URL set. wrangler is always stopped
# (trap; the whole process group, workerd included, is killed).
#
# Local only. Never runs `wrangler deploy`, `wrangler login` or anything `--remote`.
#   bash scripts/hub-test.sh smoke
# HUB_DEV_FIXTURES=0 starts the hub without DEV_FIXTURES at all (the var is not passed, and the
# top-level [vars] of wrangler.toml does not define it), for the tests that check the 404s:
#   HUB_DEV_FIXTURES=0 bash scripts/hub-test.sh smoke
set -euo pipefail

name=${1:?usage: scripts/hub-test.sh <test-name>, e.g. smoke}
[[ $name =~ ^[a-z0-9_-]+$ ]] || { echo "FAIL: bad test name: $name" >&2; exit 2; }

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
HUB_DIR=$REPO/crates/hub
[[ -f $HUB_DIR/tests/$name.rs ]] || { echo "FAIL: no such test: crates/hub/tests/$name.rs" >&2; exit 2; }

# CI-002: the wasm32 build goes to a target dir keyed by this worktree's root (wasm-<hash> under the
# shared CARGO_TARGET_DIR), so two worktrees never share hub wasm artifacts. Only worker-build and the
# wrangler dev that reruns the [build] command use it. The native cargo test below keeps the shared dir.
SHARED_TARGET_DIR=${CARGO_TARGET_DIR:-$REPO/target}
WASM_TARGET_DIR=$SHARED_TARGET_DIR/wasm-$(printf '%s' "$REPO" | sha256sum | cut -c1-16)

# RUSTFLAGS (lld) is for the native target only. `strip` breaks worker-build 0.8.7
# (docs/spikes/cloudflare.md caveat 2), so it stays off.
unset RUSTFLAGS RUSTC_WRAPPER
export CARGO_PROFILE_RELEASE_STRIP=false
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export WRANGLER_SEND_METRICS=false
# 1 (default): the hub has DEV_FIXTURES=1. 0: it has no DEV_FIXTURES at all.
HUB_DEV_FIXTURES=${HUB_DEV_FIXTURES:-1}
case $HUB_DEV_FIXTURES in
  0|1) ;;
  *) echo "FAIL: HUB_DEV_FIXTURES must be 0 or 1, got $HUB_DEV_FIXTURES" >&2; exit 2 ;;
esac
dev_fixtures_args=()
if [[ $HUB_DEV_FIXTURES == 1 ]]; then dev_fixtures_args=(--var DEV_FIXTURES:1); fi

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

echo "== worker-build --profile wasm-release ($HUB_DIR, wasm target $WASM_TARGET_DIR)"
start=$(date +%s)
# CARGO_TERM_VERBOSE makes cargo name the hub package path for fresh builds too ("Fresh hub v0.1.0 (path)").
(cd "$HUB_DIR" && CARGO_TARGET_DIR=$WASM_TARGET_DIR CARGO_TERM_VERBOSE=true worker-build --profile wasm-release) >"$STATE/build.log" 2>&1 \
  || { tail -n 40 "$STATE/build.log" >&2; fail "worker-build --profile wasm-release"; }
echo "built in $(( $(date +%s) - start ))s"
# CI-002: the bundle must come from this tree: the build log names this worktree's crates/hub.
grep -qF "($HUB_DIR)" "$STATE/build.log" \
  || { tail -n 20 "$STATE/build.log" >&2; fail "worker-build log does not name $HUB_DIR (bundle not built from this tree)"; }

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
# M2-011a: a random P-256 VAPID key per run (never a real one). node generates it in memory; the
# private scalar and the uncompressed public point (base64url, space separated) go to the hub as
# --var, like the other per-run values.
VAPID_KEYS=$(node -e '
const { generateKeyPairSync } = require("crypto");
const { privateKey, publicKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
const { d } = privateKey.export({ format: "jwk" });
const { x, y } = publicKey.export({ format: "jwk" });
const point = Buffer.concat([Buffer.from([4]), Buffer.from(x, "base64url"), Buffer.from(y, "base64url")]);
process.stdout.write(`${d} ${point.toString("base64url")}`);
')
VAPID_PRIVATE_KEY=${VAPID_KEYS% *}
VAPID_PUBLIC_KEY=${VAPID_KEYS#* }
[[ -n $VAPID_PRIVATE_KEY && -n $VAPID_PUBLIC_KEY ]] || fail "could not generate a VAPID key"

# M2-010a: harmless test values for the app-link vars (a made-up team, bundle, package and
# fingerprint, not a real app), so tests/links.rs can check the configured documents live.
export APPLE_TEAM_ID=ABCDE12345 APPLE_BUNDLE_ID=dev.sprite.machine.test
export ANDROID_PACKAGE_NAME=dev.sprite.machine.test
ANDROID_SHA256_CERT_FINGERPRINTS=$(printf 'AB:%.0s' $(seq 1 31))AB
export ANDROID_SHA256_CERT_FINGERPRINTS

# M2-003b: for the oauth suite only, a mock GitHub (served by the test process on a free local port),
# a fake client id and a random per-run client secret (never a real one). The hub gets them as --var
# (GITHUB_API_BASE points at the mock), the test process as env vars.
github_args=()
if [[ $name == oauth ]]; then
  GITHUB_CLIENT_ID=test-client-id
  GITHUB_CLIENT_SECRET=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
  MOCK_GITHUB_PORT=$(node -e 'const s=require("net").createServer().listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})')
  export GITHUB_CLIENT_ID GITHUB_CLIENT_SECRET MOCK_GITHUB_PORT
  github_args=(--var "GITHUB_CLIENT_ID:$GITHUB_CLIENT_ID" --var "GITHUB_CLIENT_SECRET:$GITHUB_CLIENT_SECRET" \
    --var "GITHUB_API_BASE:http://127.0.0.1:$MOCK_GITHUB_PORT")
fi

PORT=$(node -e 'const s=require("net").createServer().listen(0,"127.0.0.1",()=>{console.log(s.address().port);s.close()})')
BASE="http://127.0.0.1:$PORT"
echo "== wrangler dev --local --test-scheduled on port $PORT"
(cd "$HUB_DIR" && exec env HUB_WASM_TARGET_DIR="$WASM_TARGET_DIR" setsid wrangler dev --local --test-scheduled --inspector-port 0 --ip 127.0.0.1 --port "$PORT" \
  --persist-to "$STATE/persist" --var "BOOTSTRAP_OWNER_TOKEN:$BOOTSTRAP_OWNER_TOKEN" \
  --var "ATTACHMENT_SIGNING_KEY:$ATTACHMENT_SIGNING_KEY" "${dev_fixtures_args[@]}" \
  --var "VAPID_PRIVATE_KEY:$VAPID_PRIVATE_KEY" --var "VAPID_PUBLIC_KEY:$VAPID_PUBLIC_KEY" \
  --var "APPLE_TEAM_ID:$APPLE_TEAM_ID" --var "APPLE_BUNDLE_ID:$APPLE_BUNDLE_ID" \
  --var "ANDROID_PACKAGE_NAME:$ANDROID_PACKAGE_NAME" \
  --var "ANDROID_SHA256_CERT_FINGERPRINTS:$ANDROID_SHA256_CERT_FINGERPRINTS" "${github_args[@]}" \
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

echo "== cargo test -p hub --features live --test $name (HUB_URL=$BASE, DEV_FIXTURES=$HUB_DEV_FIXTURES)"
(cd "$REPO" && HUB_URL="$BASE" HUB_DEV_FIXTURES="$HUB_DEV_FIXTURES" cargo test -p hub --features live --test "$name" -- --nocapture)
echo "PASS hub-test $name"
