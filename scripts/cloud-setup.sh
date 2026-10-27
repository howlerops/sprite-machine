#!/usr/bin/env bash
# Environment setup for sprite-machine v2 cloud sessions (and local Linux dev boxes).
# Idempotent. Paste into the cloud environment's "Setup script", or run:
#   bash scripts/cloud-setup.sh
# It installs tooling only; it never reads secrets.
set -euo pipefail

SUDO=""
if [ "$(id -u)" -ne 0 ]; then SUDO="sudo"; fi

echo "==> apt packages (Vulkan/lavapipe for headless GPUI, X/Wayland libs, musl)"
export DEBIAN_FRONTEND=noninteractive
$SUDO apt-get update -qq
$SUDO apt-get install -y -qq --no-install-recommends \
  mesa-vulkan-drivers libvulkan1 libvulkan-dev vulkan-tools \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
  libfontconfig1-dev libfreetype-dev libssl-dev pkg-config \
  musl-tools xvfb xauth jq unzip curl ca-certificates >/dev/null

echo "==> rust toolchain + targets"
if ! command -v rustup >/dev/null; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
rustup component add rustfmt clippy >/dev/null
rustup target add x86_64-unknown-linux-musl wasm32-unknown-unknown wasm32-wasip2 >/dev/null

BIN="$HOME/.cargo/bin"
mkdir -p "$BIN"
fetch() { curl -fsSL --retry 4 --retry-delay 2 "$@"; }

echo "==> sccache"
if ! command -v sccache >/dev/null; then
  v=0.10.0
  fetch "https://github.com/mozilla/sccache/releases/download/v$v/sccache-v$v-x86_64-unknown-linux-musl.tar.gz" \
    | tar xz -C /tmp && install -m 0755 "/tmp/sccache-v$v-x86_64-unknown-linux-musl/sccache" "$BIN/sccache"
fi

echo "==> cargo-nextest"
if ! command -v cargo-nextest >/dev/null; then
  fetch https://get.nexte.st/latest/linux | tar xz -C "$BIN"
fi

echo "==> cargo-binstall (for prebuilt cargo tools)"
if ! command -v cargo-binstall >/dev/null; then
  fetch https://github.com/cargo-bins/cargo-binstall/releases/latest/download/cargo-binstall-x86_64-unknown-linux-musl.tgz \
    | tar xz -C "$BIN"
fi

echo "==> worker-build (Cloudflare workers-rs)"
command -v worker-build >/dev/null || cargo binstall -y --quiet worker-build || cargo install --locked worker-build

echo "==> wrangler (Cloudflare CLI)"
command -v wrangler >/dev/null || npm install -g --silent wrangler@4

echo "==> shared cargo target dir"
mkdir -p /home/user/.cargo-target-shared 2>/dev/null || mkdir -p "$HOME/.cargo-target-shared"

echo "==> verify"
vulkaninfo --summary 2>/dev/null | grep -m1 -i "deviceName" || echo "warning: no Vulkan device (lavapipe) found"
cargo --version; sccache --version; cargo nextest --version | head -1; wrangler --version; worker-build --version 2>/dev/null || true
echo "setup complete"
