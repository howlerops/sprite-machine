#!/usr/bin/env bash
# Local Linux dev toolchain for sprite-machine v2: Rust targets, Vulkan/lavapipe + Xvfb
# for the GPUI render proof, sccache, nextest, worker-build and wrangler.
# Idempotent: a second run changes nothing and exits 0. Mirrors scripts/cloud-setup.sh,
# which stays unchanged. See docs/dev.md.
#   bash scripts/setup-dev.sh
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "error: setup-dev.sh supports Linux only (see docs/dev.md)" >&2
  exit 1
fi

SUDO=""
if [ "$(id -u)" -ne 0 ]; then SUDO="sudo"; fi
export PATH="$HOME/.cargo/bin:$PATH"

# Pins. Bump deliberately, together with docs/dev.md.
WRANGLER_VERSION=4.154.0
SCCACHE_VERSION=0.10.0
NEXTEST_VERSION=0.9.150
WORKER_BUILD_VERSION=0.8.7

APT_PKGS=(
  musl-tools lld clang
  mesa-vulkan-drivers libvulkan1 vulkan-tools
  xvfb xauth imagemagick xdotool
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev
  libfontconfig-dev libssl-dev pkg-config
  curl ca-certificates
)

echo "==> apt packages (Vulkan/lavapipe, Xvfb, xdotool, ImageMagick, X/Wayland libs, musl, lld)"
missing=()
for p in "${APT_PKGS[@]}"; do
  if dpkg -s "$p" >/dev/null 2>&1; then
    echo "    $p present"
  else
    missing+=("$p")
  fi
done
if [ "${#missing[@]}" -gt 0 ]; then
  export DEBIAN_FRONTEND=noninteractive
  $SUDO apt-get update -qq
  $SUDO apt-get install -y -qq --no-install-recommends "${missing[@]}" >/dev/null
fi

echo "==> rust toolchain + targets"
if ! command -v rustup >/dev/null 2>&1; then
  curl -fsSL --retry 4 --retry-delay 2 https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
installed_targets="$(rustup target list --installed)"
for t in x86_64-unknown-linux-musl wasm32-unknown-unknown wasm32-wasip2; do
  if ! grep -qx "$t" <<<"$installed_targets"; then
    rustup target add "$t"
  fi
done

echo "==> cargo tools (sccache, cargo-nextest, worker-build)"
# cargo_install <crate> <version> <version-check command>
cargo_install() {
  local crate=$1 version=$2
  shift 2
  local out
  out="$("$@" 2>/dev/null || true)"
  if grep -q -F "$version" <<<"$out"; then
    echo "    $crate $version present"
    return 0
  fi
  cargo install --locked --version "$version" "$crate"
}
cargo_install sccache "$SCCACHE_VERSION" sccache --version
cargo_install cargo-nextest "$NEXTEST_VERSION" cargo nextest --version
cargo_install worker-build "$WORKER_BUILD_VERSION" worker-build --version

echo "==> wrangler $WRANGLER_VERSION (Cloudflare CLI, for wrangler dev)"
if ! npm ls -g --depth=0 wrangler 2>/dev/null | grep -q -F "wrangler@$WRANGLER_VERSION"; then
  npm install -g "wrangler@$WRANGLER_VERSION"
else
  echo "    wrangler $WRANGLER_VERSION present"
fi

echo "==> verify"
# Capture first: grep -q closing the pipe early would SIGPIPE vulkaninfo under pipefail.
vk="$(vulkaninfo --summary 2>/dev/null || true)"
if ! grep -qi llvmpipe <<<"$vk"; then
  echo "error: no llvmpipe (lavapipe) Vulkan device; check mesa-vulkan-drivers and vulkaninfo --summary" >&2
  exit 1
fi
grep -m1 -i "deviceName" <<<"$vk"
rustc --version
cargo --version
sccache --version
cargo nextest --version | head -n1
worker-build --version
wrangler --version | tail -n1
echo "setup-dev complete"
