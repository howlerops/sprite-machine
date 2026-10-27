#!/usr/bin/env bash
# GPUI render proof. Builds a pinned gpui-component example, runs it under xvfb-run on the
# lavapipe Vulkan driver, and screenshots the window to spikes/gpui/screenshots/<example>.png.
# Fails if the screenshot has fewer than 2 distinct colours. Run scripts/setup-dev.sh first.
#   bash scripts/gpui-render-proof.sh
set -euo pipefail

# Pinned gpui-component commit (main, 2026-10-27). Bump deliberately.
GPUI_COMPONENT_REV=c8917c3661ed1090870f403c312b39e0d8a62e0b
EXAMPLE=hello_world
MIN_COLOURS=2
XVFB_SCREEN="-screen 0 1280x800x24"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/spikes/gpui/screenshots/$EXAMPLE.png"

# Inside xvfb-run: start the app, wait for its window to paint, capture the root window,
# then kill the app. xvfb-run stops its own Xvfb when this process exits.
if [ "${1:-}" = "--in-xvfb" ]; then
  bin=$2
  out=$3
  work="$(mktemp -d)"
  log="$work/app.log"
  app_pid=""
  cleanup() {
    if [ -n "$app_pid" ]; then
      kill "$app_pid" 2>/dev/null || true
      wait "$app_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
  }
  trap cleanup EXIT INT TERM

  "$bin" >"$log" 2>&1 &
  app_pid=$!

  # Poll the root window until the app has painted something (Xvfb's root is one colour).
  painted=0
  for _ in $(seq 1 60); do
    sleep 2
    if ! kill -0 "$app_pid" 2>/dev/null; then
      echo "error: $EXAMPLE exited before painting; log:" >&2
      tail -n 40 "$log" >&2
      exit 1
    fi
    import -window root "$work/probe.png" 2>/dev/null || continue
    if [ "$(identify -format %k "$work/probe.png")" -ge "$MIN_COLOURS" ]; then
      painted=1
      break
    fi
  done
  if [ "$painted" -ne 1 ]; then
    echo "error: $EXAMPLE window never painted (no second colour after 120s); log:" >&2
    tail -n 40 "$log" >&2
    exit 1
  fi

  # Let the first full frame settle, then capture it.
  sleep 3
  mkdir -p "$(dirname "$out")"
  import -window root "$out"
  exit 0
fi

SRC="${TMPDIR:-/tmp}/gpui-component-$GPUI_COMPONENT_REV"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cargo-target-shared}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export RUSTFLAGS="${RUSTFLAGS:--C link-arg=-fuse-ld=lld}"

echo "==> gpui-component source @ ${GPUI_COMPONENT_REV:0:12}"
if [ ! -d "$SRC/.git" ] || [ "$(git -C "$SRC" rev-parse HEAD 2>/dev/null || true)" != "$GPUI_COMPONENT_REV" ]; then
  rm -rf "$SRC"
  mkdir -p "$SRC"
  git -C "$SRC" init -q
  git -C "$SRC" remote add origin https://github.com/longbridge/gpui-component
  git -C "$SRC" fetch -q --depth 1 origin "$GPUI_COMPONENT_REV"
  git -C "$SRC" checkout -q FETCH_HEAD
fi

echo "==> build $EXAMPLE"
(cd "$SRC" && cargo build -p "$EXAMPLE")
BIN="$CARGO_TARGET_DIR/debug/$EXAMPLE"
if [ ! -x "$BIN" ]; then
  echo "error: $BIN was not built" >&2
  exit 1
fi

echo "==> lavapipe ICD"
ICD="$(ls /usr/share/vulkan/icd.d/lvp_icd*.json 2>/dev/null | head -n1 || true)"
if [ -z "$ICD" ]; then
  echo "error: lavapipe ICD not found; run scripts/setup-dev.sh" >&2
  exit 1
fi
export VK_ICD_FILENAMES="$ICD"

echo "==> run $EXAMPLE under xvfb-run and capture"
xvfb-run -a -s "$XVFB_SCREEN" bash "$0" --in-xvfb "$BIN" "$OUT"

colours="$(identify -format %k "$OUT")"
if [ "$colours" -lt "$MIN_COLOURS" ]; then
  echo "error: $OUT has $colours distinct colour(s), need at least $MIN_COLOURS" >&2
  exit 1
fi
echo "PASS: $OUT ($colours distinct colours)"
