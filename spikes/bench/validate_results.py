#!/usr/bin/env python3
"""Validate benchmark evidence files against spikes/bench/results.schema.json.

    python3 -I spikes/bench/validate_results.py FILE [FILE ...]

Checks, in order:
  1. JSON Schema 2020-12 (results.schema.json).
  2. Gate id, kind, unit and threshold agree with spikes/bench/gates.yaml (threshold only
     when the device is a gates.yaml device; the example device is not checked).
  3. p50/p95 are nearest-rank percentiles of samples; value follows its statistic;
     pass == (value <= threshold) for numeric gates.
  4. Path results/<candidate>/<device>/<gate>.json agrees with the fields (when the path has
     that shape).
  5. Pinned versions required by the candidate (gates.yaml rules.version_pin).

Exit 0 when every file passes, 1 otherwise. Needs jsonschema and PyYAML (preinstalled).
"""

import json
import math
import os
import statistics
import sys

BENCH_DIR = os.path.dirname(os.path.abspath(__file__))
SCHEMA_PATH = os.path.join(BENCH_DIR, "results.schema.json")
GATES_PATH = os.path.join(BENCH_DIR, "gates.yaml")

REQUIRED_PINS = {
    "gpui-kit": {"gpui_rev", "gpui_component_rev"},
    "flutter": {"flutter", "flutter_rust_bridge"},
    "hybrid": {"gpui_rev", "gpui_component_rev", "flutter", "flutter_rust_bridge"},
}


def fail(problems, msg):
    problems.append(msg)


def nearest_rank(values, pct):
    """Nearest-rank percentile: the value at rank ceil(pct/100 * n), 1-based."""
    ordered = sorted(values)
    rank = (pct * len(ordered) + 99) // 100  # integer ceil(pct * n / 100)
    return ordered[max(rank, 1) - 1]


def close(a, b):
    return math.isclose(a, b, rel_tol=1e-9, abs_tol=1e-9)


def load_gates():
    try:
        import yaml
    except ImportError:
        return None
    with open(GATES_PATH, encoding="utf-8") as fh:
        return yaml.safe_load(fh)


def gate_threshold(gates, gate_id, device):
    """gates.yaml max for (gate, device), or None when the device has no gates.yaml key."""
    entry = next((g for g in gates["numeric"] if g["id"] == gate_id), None)
    if entry is None:
        return None
    maxes = entry["max"]
    if device in maxes:
        return maxes[device]
    if "all" in maxes:
        return maxes["all"]
    if gate_id == "binary_size":
        key = {"iphone": "ipa", "android": "apk"}.get(device)
        if device.startswith("desktop"):
            key = "desktop"
        if key in maxes:
            return maxes[key]
    return None


def check_file(path, schema_validator, gates):
    problems = []
    try:
        with open(path, encoding="utf-8") as fh:
            doc = json.load(fh)
    except (OSError, json.JSONDecodeError) as err:
        return [f"cannot read JSON: {err}"]

    for err in sorted(schema_validator.iter_errors(doc), key=lambda e: list(e.path)):
        where = "/".join(str(p) for p in err.absolute_path) or "<root>"
        problems.append(f"schema {where}: {err.message}")
    if problems:
        return problems

    # 4. path agreement: .../results/<candidate>/<device>/<gate>.json
    parts = os.path.normpath(os.path.abspath(path)).split(os.sep)
    if len(parts) >= 4 and parts[-4] == "results":
        cand, dev, gate_file = parts[-3], parts[-2], parts[-1]
        if (cand, dev, gate_file) != (doc["candidate"], doc["device"], doc["gate"] + ".json"):
            fail(problems, f"path says {cand}/{dev}/{gate_file} but fields say "
                           f"{doc['candidate']}/{doc['device']}/{doc['gate']}.json")

    # 5. pinned versions
    needed = REQUIRED_PINS.get(doc["candidate"], set())
    for key in sorted(needed - set(doc["pinned"])):
        fail(problems, f"pinned.{key} is required for candidate {doc['candidate']}")

    # 2. gates.yaml agreement
    if gates is not None:
        gate_ids = {g["id"]: "numeric" for g in gates["numeric"]}
        gate_ids.update({g["id"]: "hard" for g in gates["hard"]})
        expected_kind = gate_ids.get(doc["gate"])
        if expected_kind != doc["kind"]:
            fail(problems, f"gate {doc['gate']} is {expected_kind} in gates.yaml, file says {doc['kind']}")
        if doc["kind"] == "numeric":
            want = gate_threshold(gates, doc["gate"], doc["device"])
            if want is None:
                print(f"  note: device {doc['device']} has no gates.yaml threshold for "
                      f"{doc['gate']}; threshold not cross-checked", file=sys.stderr)
            elif not close(doc["threshold"], want):
                fail(problems, f"threshold {doc['threshold']} != gates.yaml {want}")
    else:
        print("  warning: PyYAML missing; gates.yaml cross-check skipped", file=sys.stderr)

    if doc["kind"] == "hard":
        return problems

    # 3. numeric rules
    samples = doc["samples"]
    if not close(doc["p50"], nearest_rank(samples, 50)):
        fail(problems, f"p50 {doc['p50']} != nearest-rank p50 {nearest_rank(samples, 50)}")
    if not close(doc["p95"], nearest_rank(samples, 95)):
        fail(problems, f"p95 {doc['p95']} != nearest-rank p95 {nearest_rank(samples, 95)}")

    stat = doc["statistic"]
    if stat == "median_of_run_p95":
        runs = doc.get("runs")
        if not runs or any("p95" not in r for r in runs):
            fail(problems, "median_of_run_p95 needs runs[] with p95 for every run")
        else:
            want = statistics.median(r["p95"] for r in runs)
            if not close(doc["value"], want):
                fail(problems, f"value {doc['value']} != median of run p95 {want}")
    elif stat == "median_of_runs":
        want = statistics.median(samples)
        if not close(doc["value"], want):
            fail(problems, f"value {doc['value']} != median of samples {want}")
    elif stat == "single":
        if len(samples) != 1 or not close(doc["value"], samples[0]):
            fail(problems, "single needs exactly one sample equal to value")

    if (doc["value"] <= doc["threshold"]) != doc["pass"]:
        fail(problems, f"pass={doc['pass']} but value {doc['value']} vs threshold {doc['threshold']}")
    return problems


def main(argv):
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        import jsonschema
    except ImportError:
        print("error: jsonschema is required", file=sys.stderr)
        return 2
    with open(SCHEMA_PATH, encoding="utf-8") as fh:
        schema = json.load(fh)
    jsonschema.Draft202012Validator.check_schema(schema)
    validator = jsonschema.Draft202012Validator(
        schema, format_checker=jsonschema.FormatChecker())
    gates = load_gates()

    status = 0
    for path in argv:
        problems = check_file(path, validator, gates)
        if problems:
            status = 1
            print(f"FAIL {path}")
            for msg in problems:
                print(f"  - {msg}")
        else:
            print(f"OK   {path}")
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
