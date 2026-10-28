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

# WEB-004: a random per-run P-256 VAPID key (never a real one), so the hub can sign pushes to the
# test's fake push service; as scripts/hub-test.sh does.
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

# WEB-006: a mock GitHub on a free local port (never github.com), a fake client id and a random
# per-run client secret (never a real one). The hub gets them as --var; GITHUB_API_BASE points at
# the mock. Only the GitHub sign-in and members specs use it.
GITHUB_CLIENT_ID=test-client-id
GITHUB_CLIENT_SECRET=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
MOCK_GITHUB_PORT=$(node -e 'const s=require("net").createServer().listen(0,"127.0.0.1",()=>{process.stdout.write(String(s.address().port));s.close()})')

STATE=$(mktemp -d)
WRANGLER_PID=""
MOCK_PID=""
scratch_created=0
[[ -e $HUB_DIR/.wrangler ]] || scratch_created=1

cleanup() {
  trap - EXIT INT TERM
  if [[ -n $MOCK_PID ]]; then kill -TERM "$MOCK_PID" 2>/dev/null || true; fi
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

node "$REPO/web/tests/helpers-ghmock.mjs" "$MOCK_GITHUB_PORT" "$GITHUB_CLIENT_ID" "$GITHUB_CLIENT_SECRET" &
MOCK_PID=$!

# MVP-000h: the wrangler [build] strip step runs a prebuilt cli binary by path, never cargo. Build
# cli before wrangler starts and give wrangler a per-run copy (as scripts/hub-test.sh does).
(cd "$REPO" && cargo build -q -p cli) || { echo "FAIL: cargo build -p cli" >&2; exit 1; }
STRIP_BIN=$STATE/cli-strip
cp "${CARGO_TARGET_DIR:-$REPO/target}/debug/cli" "$STRIP_BIN" || { echo "FAIL: copy the cli binary" >&2; exit 1; }

(cd "$HUB_DIR" && exec env HUB_WASM_TARGET_DIR="$WASM_TARGET_DIR" HUB_STRIP_BIN="$STRIP_BIN" setsid wrangler dev --local --ip 127.0.0.1 --port "$SM_PORT" \
  --persist-to "$STATE/persist" \
  --var "BOOTSTRAP_OWNER_TOKEN:$BOOTSTRAP_OWNER_TOKEN" \
  --var "ATTACHMENT_SIGNING_KEY:$ATTACHMENT_SIGNING_KEY" \
  --var "DEV_FIXTURES:1" \
  --var "VAPID_PRIVATE_KEY:$VAPID_PRIVATE_KEY" --var "VAPID_PUBLIC_KEY:$VAPID_PUBLIC_KEY" \
  --var "GITHUB_CLIENT_ID:$GITHUB_CLIENT_ID" --var "GITHUB_CLIENT_SECRET:$GITHUB_CLIENT_SECRET" \
  --var "GITHUB_API_BASE:http://127.0.0.1:$MOCK_GITHUB_PORT" \
  --show-interactive-dev-session=false) &
WRANGLER_PID=$!
wait "$WRANGLER_PID"
