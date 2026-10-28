#!/usr/bin/env bash
# WEB-001: start the hub for the browser tests, like scripts/hub-test.sh: apply the local D1
# migrations into a throwaway state dir, then `wrangler dev --local` on $SM_PORT with random
# per-run secrets (BOOTSTRAP_OWNER_TOKEN, ATTACHMENT_SIGNING_KEY from the environment, set by
# playwright.config.js) and DEV_FIXTURES=1. Runs until it gets SIGTERM; then wrangler's whole
# process group (workerd included) is killed and the state dir removed.
#
# Local only. Never runs `wrangler deploy`, `wrangler login` or anything `--remote`.
set -euo pipefail

: "${SM_PORT:?}" "${BOOTSTRAP_OWNER_TOKEN:?}" "${ATTACHMENT_SIGNING_KEY:?}"

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
HUB_DIR=$REPO/crates/hub

# CI-002: the wasm32 build goes to a target dir keyed by this worktree's root, like
# scripts/hub-test.sh, so another worktree's build never replaces the hub this run serves.
WASM_TARGET_DIR=${CARGO_TARGET_DIR:-$REPO/target}/wasm-$(printf '%s' "$REPO" | sha256sum | cut -c1-16)

# RUSTFLAGS (lld) is for the native target only; `strip` breaks worker-build 0.8.7.
unset RUSTFLAGS RUSTC_WRAPPER
export CARGO_PROFILE_RELEASE_STRIP=false
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export WRANGLER_SEND_METRICS=false

STATE=$(mktemp -d)
WRANGLER_PID=""
scratch_created=0
[[ -e $HUB_DIR/.wrangler ]] || scratch_created=1

cleanup() {
  trap - EXIT INT TERM
  if [[ -n $WRANGLER_PID ]]; then
    kill -TERM -- "-$WRANGLER_PID" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$WRANGLER_PID" 2>/dev/null || break
      sleep 0.5
    done
    kill -KILL -- "-$WRANGLER_PID" 2>/dev/null || true
    wait "$WRANGLER_PID" 2>/dev/null || true
  fi
  if (( scratch_created )); then rm -rf "$HUB_DIR/.wrangler"; fi
  rm -rf "$STATE"
}
trap 'cleanup; exit 0' TERM INT
trap cleanup EXIT

(cd "$HUB_DIR" && wrangler d1 migrations apply DB --local --persist-to "$STATE/persist") \
  >"$STATE/d1.log" 2>&1 || { cat "$STATE/d1.log" >&2; echo "FAIL: d1 migrations apply" >&2; exit 1; }

(cd "$HUB_DIR" && exec env HUB_WASM_TARGET_DIR="$WASM_TARGET_DIR" setsid wrangler dev --local --ip 127.0.0.1 --port "$SM_PORT" \
  --persist-to "$STATE/persist" \
  --var "BOOTSTRAP_OWNER_TOKEN:$BOOTSTRAP_OWNER_TOKEN" \
  --var "ATTACHMENT_SIGNING_KEY:$ATTACHMENT_SIGNING_KEY" \
  --var "DEV_FIXTURES:1" \
  --show-interactive-dev-session=false) &
WRANGLER_PID=$!
wait "$WRANGLER_PID"
