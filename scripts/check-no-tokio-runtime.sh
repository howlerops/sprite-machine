#!/usr/bin/env bash
# Guard for the "no tokio runtime on Workers" rule (plan §4.2, decided after spike M0-017):
# in the wasm32 dependency tree of the given package, tokio may appear only as a direct
# dependency of workers-rs `worker` (which needs it for AsyncRead/AsyncWrite traits) and with
# NO features enabled (so no rt, net, time, sync or macros).
#   bash scripts/check-no-tokio-runtime.sh hub
set -euo pipefail
pkg=${1:?usage: check-no-tokio-runtime.sh <package>}
tree=(cargo tree -p "$pkg" --target wasm32-unknown-unknown -e normal)
lines=$("${tree[@]}" -f '{p} [{f}]' --prefix none | grep -E '^tokio ' | sort -u || true)
if [[ -z "$lines" ]]; then
  echo "ok: no tokio in $pkg (wasm32)"
  exit 0
fi
if ! [[ "$lines" =~ ^tokio\ v[0-9.]+\ \[\]$ ]]; then
  echo "FAIL: tokio in $pkg has features enabled: $lines" >&2
  exit 1
fi
dependents=$("${tree[@]}" -i tokio --depth 1 --prefix none | sed 1d | sed 's/ (\*)$//' | sort -u)
if [[ -n "$(grep -vE '^worker v' <<<"$dependents" || true)" ]]; then
  echo "FAIL: tokio pulled into $pkg by something other than worker: $dependents" >&2
  exit 1
fi
echo "ok: tokio in $pkg only via worker, no features (no runtime)"
