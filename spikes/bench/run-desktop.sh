#!/usr/bin/env bash
# Desktop (Linux) benchmark for the gpui-kit candidate, per spikes/bench/gates.yaml.
#
#   bash spikes/bench/run-desktop.sh --dry-run    print every step; build and run nothing; exit 0
#   bash spikes/bench/run-desktop.sh              real run (release build, ~GBs of disk; see below)
#   options: --keep-work   keep the scratch directory with raw logs
#
# Real mode:
#   1. preflight: cargo, strip, xvfb-run, xdotool, import/identify, python3, lavapipe ICD.
#   2. build the gpui spike in release (spikes/gpui, standalone workspace) and agui-fixture
#      in release (root workspace), with the pinned gpui-component rev from the root Cargo.toml.
#   3. binary_size: strip a copy of the release binary and measure it (decimal MB).
#   4. for each of gates.yaml workload.runs (5) runs: start agui-fixture
#      `--scenario transcript-10k --rate 200 --addr 127.0.0.1:<free port>`, then run the spike
#      under `xvfb-run -s "-screen 0 1280x800x24"` on lavapipe. Inside xvfb the wrapper:
#        - takes the pre-exec wall clock (cold start starts here), starts the spike;
#        - samples the spike's VmHWM from /proc/<pid>/status every 200 ms (rss_peak);
#        - waits for the spike's first-interactive-frame marker (cold_start);
#        - drives input with xdotool for the workload length: wheel-scrolls the transcript
#          and sends one key every 200 ms to the composer (key_to_glyph_p95).
#   5. summarize_desktop.py writes spikes/bench/results/gpui-kit/desktop_linux/*.json (the five
#      numeric gates, with raw samples and pinned versions) and validates them.
#
# Instrumentation the gpui spike MUST provide (none exists yet; proposed as ledger items in
# docs/ledger/reports/M1B-008.md). Environment variables are the only inputs:
#   GPUI_SPIKE_FIXTURE_ADDR=<host:port>
#       Render the transcript from ws://<addr>/threads/th_fixture/subscribe?after=0 instead of
#       the in-memory fixture rows. Also render the common workload: a 5k-line diff open, a
#       terminal receiving output, one pending approval card.
#   GPUI_SPIKE_BENCH_LOG=<path>
#       Append JSON lines (one object per line, microseconds since the Unix epoch, CLOCK_REALTIME):
#         {"kind":"frame","t_us":<u64>,"dt_us":<u64>}
#             one per presented frame; t_us = presentation time, dt_us = interval since the
#             previous frame (this is the frame time the gate measures)
#         {"kind":"key_to_glyph","id":<u64>,"key_us":<u64>,"glyph_us":<u64>}
#             one per key delivered to the composer; key_us = when the app receives the KeyDown,
#             glyph_us = when the frame that paints the new glyph is presented
#   Stdout (must be flushed, must appear exactly once per run):
#         BENCH_MARKER first_interactive_frame unix_us=<u64>
#       printed when the shell's first interactive frame (transcript visible, composer focused)
#       is presented. Cold start = (unix_us - pre-exec time) / 1e6 seconds.
#   Window title "gpui-spike"; the composer has keyboard focus at startup (xdotool targets both).
#
# Known limits of the desktop run: keys are injected through XTEST, so key_to_glyph starts at
# the app's KeyDown receipt and excludes X-server dispatch; the first-interactive marker is the
# transcript shell, not the inbox (the inbox does not exist in the spike yet).
#
# Verify (M1B-008): --dry-run only. The release build needs more disk than this container has.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BENCH="$ROOT/spikes/bench"
SELF="$BENCH/run-desktop.sh"
CANDIDATE="gpui-kit"
DEVICE="desktop_linux"
FIXTURE_SCENARIO="transcript-10k"
KEY_INTERVAL_S=0.2
MARKER_TIMEOUT_S=120
WINDOW_TITLE="gpui-spike"
XVFB_SCREEN="-screen 0 1280x800x24"

DRY_RUN=0
KEEP_WORK=0
FIXTURE_PID=""
WORK=""

say() { printf '\n==> %s\n' "$*"; }
dry() { printf '    [dry-run] %s\n' "$*"; }

# Workload and runs come from gates.yaml (the normative file), not from this script.
read_workload() {
  local line
  line="$(python3 -I -c '
import yaml, sys
w = yaml.safe_load(open(sys.argv[1], encoding="utf-8"))["workload"]
print(w["runs"], w["events_per_second"], w["transcript_events"], w["diff_lines"])
' "$BENCH/gates.yaml")"
  read -r RUNS EVENTS_PER_SECOND TRANSCRIPT_EVENTS DIFF_LINES <<<"$line"
  RUN_SECONDS=$(( TRANSCRIPT_EVENTS / EVENTS_PER_SECOND ))
}

# Pinned versions, read from the manifests (gates.yaml rules.version_pin).
read_pins() {
  GPUI_COMPONENT_REV="$(sed -n 's/^gpui-component = .*rev = "\([0-9a-f]\{40\}\)".*/\1/p' "$ROOT/Cargo.toml" | head -n1)"
  GPUI_VERSION="$(sed -n 's/^gpui = { package = "gpui-pre", version = "=\([^"]*\)".*/\1/p' "$ROOT/Cargo.toml" | head -n1)"
  if [ -z "$GPUI_COMPONENT_REV" ] || [ -z "$GPUI_VERSION" ]; then
    echo "error: cannot read gpui pins from $ROOT/Cargo.toml" >&2
    exit 1
  fi
  GPUI_REV="gpui-pre@$GPUI_VERSION"
  TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cargo-target-shared}"  # same default as gpui-render-proof.sh
  SPIKE_BIN="$TARGET_DIR/release/gpui-spike"
  FIXTURE_BIN="$TARGET_DIR/release/agui-fixture"
  ICD="${ICD:-}"
}

free_port() {
  python3 -I -c 'import socket
s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

wait_for_port() {
  local host="$1" port="$2" tries="$3"
  while [ "$tries" -gt 0 ]; do
    if (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; then return 0; fi
    sleep 0.5
    tries=$((tries - 1))
  done
  echo "error: nothing listening on $host:$port" >&2
  return 1
}

preflight() {
  say "preflight: tools, lavapipe ICD, pins"
  if [ "$DRY_RUN" -eq 1 ]; then
    dry "check commands: cargo strip xvfb-run xdotool import identify python3 awk"
    dry "check lavapipe ICD: /usr/share/vulkan/icd.d/lvp_icd*.json (scripts/setup-dev.sh installs it)"
    dry "pins: gpui-component rev $GPUI_COMPONENT_REV, gpui $GPUI_REV (root Cargo.toml)"
    return 0
  fi
  local missing=() t
  for t in cargo strip xvfb-run xdotool import identify python3 awk timeout; do
    command -v "$t" >/dev/null 2>&1 || missing+=("$t")
  done
  ICD="$(ls /usr/share/vulkan/icd.d/lvp_icd*.json 2>/dev/null | head -n1 || true)"
  [ -n "$ICD" ] || missing+=("lavapipe-ICD(run scripts/setup-dev.sh)")
  if [ "${#missing[@]}" -gt 0 ]; then
    echo "error: missing: ${missing[*]}" >&2
    exit 1
  fi
  echo "    lavapipe ICD: $ICD"
  echo "    pins: gpui-component $GPUI_COMPONENT_REV, gpui $GPUI_REV"
}

build() {
  say "build release binaries (gpui spike: spikes/gpui; agui-fixture: root workspace)"
  if [ "$DRY_RUN" -eq 1 ]; then
    dry "CARGO_TARGET_DIR=$TARGET_DIR CARGO_BUILD_JOBS=\${CARGO_BUILD_JOBS:-2} RUSTFLAGS=\${RUSTFLAGS:--C link-arg=-fuse-ld=lld}"
    dry "cargo build --release --manifest-path $ROOT/spikes/gpui/Cargo.toml --bin gpui-spike"
    dry "cargo build --release --manifest-path $ROOT/Cargo.toml -p agui-fixture"
    dry "check $SPIKE_BIN and $FIXTURE_BIN exist"
    return 0
  fi
  export CARGO_TARGET_DIR="$TARGET_DIR"
  export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
  export RUSTFLAGS="${RUSTFLAGS:--C link-arg=-fuse-ld=lld}"
  cargo build --release --manifest-path "$ROOT/spikes/gpui/Cargo.toml" --bin gpui-spike
  cargo build --release --manifest-path "$ROOT/Cargo.toml" -p agui-fixture
  for bin in "$SPIKE_BIN" "$FIXTURE_BIN"; do
    [ -x "$bin" ] || { echo "error: $bin was not built" >&2; exit 1; }
  done
}

binary_size() {
  say "binary_size: stripped copy of the release spike binary"
  if [ "$DRY_RUN" -eq 1 ]; then
    dry "strip -o $WORK/runs/gpui-spike.stripped $SPIKE_BIN"
    dry "stat -c %s $WORK/runs/gpui-spike.stripped > $WORK/runs/binary_bytes.txt"
    return 0
  fi
  mkdir -p "$WORK/runs"
  strip -o "$WORK/runs/gpui-spike.stripped" "$SPIKE_BIN"
  stat -c %s "$WORK/runs/gpui-spike.stripped" > "$WORK/runs/binary_bytes.txt"
  echo "    $(cat "$WORK/runs/binary_bytes.txt") bytes"
}

# Runs inside xvfb-run: $1 = spike binary, $2 = run dir, $3 = fixture host:port.
sample_rss() {
  local pid="$1" out="$2" peak=0 v
  while kill -0 "$pid" 2>/dev/null; do
    v="$(awk '/^VmHWM:/ {print $2}' "/proc/$pid/status" 2>/dev/null || true)"
    if [ -n "$v" ] && [ "$v" -gt "$peak" ]; then
      peak="$v"
      echo "$peak" > "$out"
    fi
    sleep 0.2
  done
}

drive_input() {
  local pid="$1" run_dir="$2" wid end keys=0
  wid="$(timeout 60 xdotool search --sync --limit 1 --name "^${WINDOW_TITLE}\$")"
  xdotool windowfocus --sync "$wid"
  end=$(( $(date +%s) + RUN_SECONDS ))
  while [ "$(date +%s)" -lt "$end" ]; do
    kill -0 "$pid" 2>/dev/null || { echo "error: spike exited during the run" >&2; return 1; }
    xdotool mousemove --window "$wid" 400 300 click --repeat 3 --delay 20 5   # wheel down
    xdotool key --delay 0 a                                                    # composer key
    keys=$((keys + 1))
    sleep "$KEY_INTERVAL_S"
  done
  echo "input: $keys key events over ${RUN_SECONDS}s" >> "$run_dir/input.log"
}

in_xvfb() {
  local bin="$1" run_dir="$2" fixture_addr="$3"
  local log="$run_dir/app.log" frames="$run_dir/frames.jsonl" rss="$run_dir/rss.txt"
  local pid sampler waited=0 limit=$(( MARKER_TIMEOUT_S * 5 ))
  mkdir -p "$run_dir"
  : > "$frames"
  date +%s%6N > "$run_dir/start_us.txt"          # pre-exec wall clock (cold start origin)
  GPUI_SPIKE_FIXTURE_ADDR="$fixture_addr" GPUI_SPIKE_BENCH_LOG="$frames" \
    "$bin" > "$log" 2>&1 &
  pid=$!
  trap 'kill "$pid" 2>/dev/null || true' EXIT
  sample_rss "$pid" "$rss" &
  sampler=$!
  until grep -q '^BENCH_MARKER first_interactive_frame ' "$log" 2>/dev/null; do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "error: spike exited before its first frame; log tail:" >&2
      tail -n 40 "$log" >&2
      return 1
    fi
    if [ "$waited" -ge "$limit" ]; then
      echo "error: no first_interactive_frame marker after ${MARKER_TIMEOUT_S}s" >&2
      return 1
    fi
    sleep 0.2
    waited=$((waited + 1))
  done
  drive_input "$pid" "$run_dir"
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  wait "$sampler" 2>/dev/null || true
  [ -s "$rss" ] || { echo "error: no VmHWM sample in $rss" >&2; return 1; }
}

run_benchmarks() {
  say "workload: agui-fixture $FIXTURE_SCENARIO at $EVENTS_PER_SECOND ev/s; $RUNS runs of ${RUN_SECONDS}s; common workload = ${DIFF_LINES}-line diff, terminal output, one approval card"
  local i run_dir port
  for i in $(seq 1 "$RUNS"); do
    run_dir="$WORK/runs/run$i"
    say "run $i of $RUNS"
    if [ "$DRY_RUN" -eq 1 ]; then
      dry "port=\$(free loopback port)"
      dry "$FIXTURE_BIN --scenario $FIXTURE_SCENARIO --rate $EVENTS_PER_SECOND --addr 127.0.0.1:\$port > $run_dir/fixture.log"
      dry "wait until 127.0.0.1:\$port accepts connections"
      dry "VK_ICD_FILENAMES=\$ICD xvfb-run -a -s \"$XVFB_SCREEN\" bash $SELF --in-xvfb $SPIKE_BIN $run_dir 127.0.0.1:\$port"
      dry "    inside xvfb: start spike (GPUI_SPIKE_FIXTURE_ADDR, GPUI_SPIKE_BENCH_LOG=$run_dir/frames.jsonl); sample VmHWM -> $run_dir/rss.txt;"
      dry "    wait for 'BENCH_MARKER first_interactive_frame' in $run_dir/app.log; xdotool: scroll + one key per ${KEY_INTERVAL_S}s for ${RUN_SECONDS}s; kill spike"
      dry "stop agui-fixture"
      continue
    fi
    mkdir -p "$run_dir"
    port="$(free_port)"
    "$FIXTURE_BIN" --scenario "$FIXTURE_SCENARIO" --rate "$EVENTS_PER_SECOND" \
      --addr "127.0.0.1:$port" > "$run_dir/fixture.log" 2>&1 &
    FIXTURE_PID=$!
    wait_for_port 127.0.0.1 "$port" 60
    VK_ICD_FILENAMES="$ICD" xvfb-run -a -s "$XVFB_SCREEN" \
      bash "$SELF" --in-xvfb "$SPIKE_BIN" "$run_dir" "127.0.0.1:$port"
    kill "$FIXTURE_PID" 2>/dev/null || true
    wait "$FIXTURE_PID" 2>/dev/null || true
    FIXTURE_PID=""
  done
}

summarize() {
  local out="$BENCH/results/$CANDIDATE/$DEVICE"
  local os_version model commit notes
  os_version="$(. /etc/os-release && echo "$PRETTY_NAME") kernel $(uname -r)"
  model="$(cat /sys/devices/virtual/dmi/id/product_name 2>/dev/null || echo unknown)"
  commit="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  notes="spike commit $commit; release build; xvfb-run -s \"$XVFB_SCREEN\"; renderer lavapipe (VK_ICD_FILENAMES=$ICD); fixture $FIXTURE_SCENARIO at $EVENTS_PER_SECOND ev/s"
  if [ "$DRY_RUN" -eq 1 ]; then
    dry "python3 -I $BENCH/summarize_desktop.py --runs-dir $WORK/runs --out-dir $out --candidate $CANDIDATE --device $DEVICE --device-model '<model>' --os-version '<os>' --date $(date +%F) --gpui-rev $GPUI_REV --gpui-component-rev $GPUI_COMPONENT_REV"
    dry "writes $out/{frame_time_p95,key_to_glyph_p95,cold_start,rss_peak,binary_size}.json and validates them"
    return 0
  fi
  say "summarize and validate -> ${out#"$ROOT"/}"
  python3 -I "$BENCH/summarize_desktop.py" \
    --runs-dir "$WORK/runs" --out-dir "$out" \
    --candidate "$CANDIDATE" --device "$DEVICE" \
    --device-model "$model" --os-version "$os_version" --date "$(date +%F)" \
    --gpui-rev "$GPUI_REV" --gpui-component-rev "$GPUI_COMPONENT_REV" \
    --notes "$notes"
}

cleanup() {
  if [ -n "$FIXTURE_PID" ]; then kill "$FIXTURE_PID" 2>/dev/null || true; fi
  if [ -n "$WORK" ] && [ "$KEEP_WORK" -eq 0 ] && [ "$DRY_RUN" -eq 0 ]; then rm -rf "$WORK"; fi
}

usage() {
  sed -n '2,6p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

# Internal entry point, run by xvfb-run for each benchmark run.
if [ "${1:-}" = "--in-xvfb" ]; then
  shift
  read_workload
  in_xvfb "$@"
  exit $?
fi

for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --keep-work) KEEP_WORK=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $arg" >&2; usage >&2; exit 2 ;;
  esac
done

read_workload
read_pins

say "run-desktop: candidate=$CANDIDATE device=$DEVICE mode=$([ "$DRY_RUN" -eq 1 ] && echo dry-run || echo real)"
echo "    gates.yaml workload: runs=$RUNS events_per_second=$EVENTS_PER_SECOND transcript_events=$TRANSCRIPT_EVENTS diff_lines=$DIFF_LINES run_seconds=$RUN_SECONDS"
echo "    gates: frame_time_p95 key_to_glyph_p95 cold_start rss_peak binary_size (desktop_linux)"

if [ "$DRY_RUN" -eq 1 ]; then
  WORK="<scratch dir from mktemp -d>"
else
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/bench-desktop.XXXXXX")"
  trap cleanup EXIT
fi

preflight
build
binary_size
run_benchmarks
summarize

if [ "$DRY_RUN" -eq 1 ]; then
  say "dry-run complete: nothing was built, run or written"
fi
