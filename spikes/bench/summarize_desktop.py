#!/usr/bin/env python3
"""Turn raw run artifacts from run-desktop.sh into the five desktop evidence files.

    python3 -I spikes/bench/summarize_desktop.py --runs-dir DIR --out-dir DIR [options]

Input, per run DIR/run<i>/ (written by run-desktop.sh):
  start_us.txt   pre-exec wall clock, unix microseconds
  app.log        spike stdout/stderr; contains `BENCH_MARKER first_interactive_frame unix_us=<n>`
  frames.jsonl   spike bench log: {"kind":"frame","t_us","dt_us"} and
                 {"kind":"key_to_glyph","id","key_us","glyph_us"}, one JSON object per line
  rss.txt        peak VmHWM of the spike process, in kB (from /proc/<pid>/status)
Plus DIR/binary_bytes.txt (size of the stripped release binary).

Output (in --out-dir): frame_time_p95.json, key_to_glyph_p95.json, cold_start.json,
rss_peak.json, binary_size.json, each checked by validate_results.py, and raw/ (a copy of the
run artifacts, referenced from each file's evidence list).

Statistics follow the schema: p50/p95 are nearest-rank over pooled samples; the gated value is
the median over runs (median of each run's p95 for frame time and key-to-glyph; median of one
value per run for cold start and RSS). Units: ms, s, decimal MB (10^6 bytes). Samples are
rounded to 4 decimals once, at load time, and every derived number uses the rounded values.
"""

import argparse
import glob
import json
import os
import re
import shutil
import statistics
import subprocess
import sys

BENCH_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(os.path.dirname(BENCH_DIR))


def nearest_rank(values, pct):
    ordered = sorted(values)
    rank = (pct * len(ordered) + 99) // 100
    return ordered[max(rank, 1) - 1]


def read_text(path):
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def load_run(run_dir):
    start_us = int(read_text(os.path.join(run_dir, "start_us.txt")).strip())
    log = read_text(os.path.join(run_dir, "app.log"))
    m = re.search(r"^BENCH_MARKER first_interactive_frame unix_us=(\d+)$", log, re.M)
    if not m:
        raise SystemExit(f"error: {run_dir}/app.log has no first_interactive_frame marker")
    marker_us = int(m.group(1))
    frames, keys = [], []
    with open(os.path.join(run_dir, "frames.jsonl"), encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            if rec.get("kind") == "frame" and rec["t_us"] >= marker_us:
                frames.append(round(rec["dt_us"] / 1000.0, 4))  # interval between presents, ms
            elif rec.get("kind") == "key_to_glyph":
                keys.append(round((rec["glyph_us"] - rec["key_us"]) / 1000.0, 4))
    peak_kb = int(read_text(os.path.join(run_dir, "rss.txt")).strip())
    return {
        "cold_start_s": round((marker_us - start_us) / 1e6, 4),
        "frames_ms": frames,
        "keys_ms": keys,
        "rss_mb": round(peak_kb * 1024 / 1e6, 4),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--runs-dir", required=True)
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--candidate", required=True)
    ap.add_argument("--device", required=True)
    ap.add_argument("--device-model", required=True)
    ap.add_argument("--os-version", required=True)
    ap.add_argument("--date", required=True)
    ap.add_argument("--gpui-rev", required=True)
    ap.add_argument("--gpui-component-rev", required=True)
    ap.add_argument("--notes", default="")
    args = ap.parse_args()

    import yaml
    with open(os.path.join(BENCH_DIR, "gates.yaml"), encoding="utf-8") as fh:
        gates = yaml.safe_load(fh)
    numeric = {g["id"]: g["max"] for g in gates["numeric"]}

    def threshold(gate):
        maxes = numeric[gate]
        if args.device in maxes:
            return maxes[args.device]
        if "all" in maxes:
            return maxes["all"]
        return maxes["desktop"]  # binary_size keys are ipa / apk / desktop

    run_dirs = sorted(glob.glob(os.path.join(args.runs_dir, "run*")))
    if len(run_dirs) != gates["workload"]["runs"]:
        raise SystemExit(f"error: expected {gates['workload']['runs']} runs, found {len(run_dirs)}")
    runs = [load_run(d) for d in run_dirs]
    binary_mb = round(int(read_text(os.path.join(args.runs_dir, "binary_bytes.txt")).strip()) / 1e6, 4)

    os.makedirs(args.out_dir, exist_ok=True)
    raw_dir = os.path.join(args.out_dir, "raw")
    shutil.rmtree(raw_dir, ignore_errors=True)
    shutil.copytree(args.runs_dir, raw_dir)
    evidence = [os.path.relpath(raw_dir, REPO_ROOT)]
    notes_tail = f" {args.notes}".rstrip()

    def doc(gate, unit, statistic, samples, value, runs_field, passed, notes):
        limit = threshold(gate)
        return gate, {
            "schema_version": 1,
            "candidate": args.candidate,
            "device": args.device,
            "device_model": args.device_model,
            "os_version": args.os_version,
            "date": args.date,
            "pinned": {"gpui_rev": args.gpui_rev, "gpui_component_rev": args.gpui_component_rev},
            "gate": gate,
            "kind": "numeric",
            "unit": unit,
            "threshold": limit,
            "statistic": statistic,
            "value": value,
            "samples": samples,
            "p50": nearest_rank(samples, 50),
            "p95": nearest_rank(samples, 95),
            **({"runs": runs_field} if runs_field is not None else {}),
            "pass": value <= limit,
            "evidence": evidence,
            "notes": notes + notes_tail,
        }

    written = []
    # Frame time and key-to-glyph: pooled raw values; gated value = median over runs of run p95.
    for gate, key, what in (
            ("frame_time_p95", "frames_ms", "frame time (interval between presents)"),
            ("key_to_glyph_p95", "keys_ms", "key event receipt to glyph in composer")):
        pooled = [v for r in runs for v in r[key]]
        if not pooled:
            raise SystemExit(f"error: no {what} samples in any run")
        per_run = [{"run": i + 1, "n": len(r[key]),
                    "p50": nearest_rank(r[key], 50), "p95": nearest_rank(r[key], 95)}
                   for i, r in enumerate(runs)]
        value = statistics.median(p["p95"] for p in per_run)
        written.append(doc(gate, "ms", "median_of_run_p95", pooled, value, per_run, None,
                           f"{what}; agui-fixture transcript-10k at 200 ev/s; median of "
                           f"{len(runs)} runs of each run's own p95."))

    cold = [r["cold_start_s"] for r in runs]
    per_run = [{"run": i + 1, "n": 1, "value": v} for i, v in enumerate(cold)]
    written.append(doc("cold_start", "s", "median_of_runs", cold, statistics.median(cold),
                       per_run, None,
                       "process start (wrapper pre-exec timestamp) to first interactive frame; "
                       f"median of {len(runs)} runs."))

    rss = [r["rss_mb"] for r in runs]
    per_run = [{"run": i + 1, "n": 1, "value": v} for i, v in enumerate(rss)]
    written.append(doc("rss_peak", "MB", "median_of_runs", rss, statistics.median(rss),
                       per_run, None,
                       f"peak VmHWM of the spike process; median of {len(runs)} runs."))

    written.append(doc("binary_size", "MB", "single", [binary_mb], binary_mb, None, None,
                       "stripped release binary (strip -o copy of gpui-spike), decimal MB."))

    files = []
    for gate, body in written:
        path = os.path.join(args.out_dir, f"{gate}.json")
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(body, fh, indent=2)
            fh.write("\n")
        files.append(path)
        print(f"wrote {os.path.relpath(path, REPO_ROOT)}: value={body['value']} {body['unit']} "
              f"threshold={body['threshold']} pass={body['pass']}")

    validator = os.path.join(BENCH_DIR, "validate_results.py")
    return subprocess.run([sys.executable, "-I", validator, *files], check=False).returncode


if __name__ == "__main__":
    sys.exit(main())
