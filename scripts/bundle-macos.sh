#!/usr/bin/env bash
# Assemble an unsigned "Sprite Machine.app" around an already-built client-gpui binary.
# No signing, no notarization, no secrets. Runs on Linux too (layout check only).
#
# Usage: scripts/bundle-macos.sh --binary PATH [--out DIR] [--version X.Y.Z] [--sha SHA]
#                                [--min-macos 13.0] [--bundle-id ID]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$here/.."
template="$root/crates/client-gpui/bundle/Info.plist.in"

binary=""
out="target/bundle"
version=""
sha=""
min_macos="13.0"
bundle_id="app.spritemachine.desktop"
exe_name="client-gpui"

while [ $# -gt 0 ]; do
  case "$1" in
    --binary) binary="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    --sha) sha="$2"; shift 2 ;;
    --min-macos) min_macos="$2"; shift 2 ;;
    --bundle-id) bundle_id="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$binary" ] && [ -f "$binary" ] || { echo "--binary must point to a built executable" >&2; exit 2; }
if [ -z "$version" ]; then
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
fi
[ -n "$version" ] || { echo "could not determine version" >&2; exit 2; }
if [ -z "$sha" ]; then
  sha="$(git -C "$root" rev-parse --short=7 HEAD 2>/dev/null || echo unknown)"
fi
case "$version$sha$bundle_id$min_macos" in
  *[\&\|\\\<\>\"]*) echo "version/sha/id contain characters unsafe for the plist" >&2; exit 2 ;;
esac

app="$out/Sprite Machine.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/$exe_name"
chmod 755 "$app/Contents/MacOS/$exe_name"

sed \
  -e "s|@EXECUTABLE@|$exe_name|g" \
  -e "s|@BUNDLE_ID@|$bundle_id|g" \
  -e "s|@SHORT_VERSION@|$version+$sha|g" \
  -e "s|@BUILD_VERSION@|$version|g" \
  -e "s|@MIN_MACOS@|$min_macos|g" \
  "$template" > "$app/Contents/Info.plist"

printf 'APPL????' > "$app/Contents/PkgInfo"
echo "$app"
