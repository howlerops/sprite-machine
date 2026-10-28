# MVP-000h report: the wasm strip step runs a prebuilt binary

## Summary

- The wrangler `[build]` strip step no longer runs cargo. It calls `${HUB_STRIP_BIN:-${CARGO_TARGET_DIR:-../../target}/debug/cli}`, so nothing waits on the shared target lock.
- `scripts/hub-test.sh` and `web/tests/helpers-hub.sh` build `cli` before wrangler starts and copy it to a per-run temp file, which they pass as `HUB_STRIP_BIN`.
- The tracer (`Wrangler::start`) copies its own running `cli` binary to the run's temp dir and passes that as `HUB_STRIP_BIN`.
- Verified: `hub-test.sh smoke`, `e2e tracer` and clippy all exit 0. Wrangler reached Ready with `HUB_STRIP_BIN` set, about 16 to 20 s after start.
- Not verified: the manual check with a held cargo lock (the permission classifier denied it), and `helpers-hub.sh` (Playwright was not run).

## Files changed

- `crates/hub/wrangler.toml`: `[build] command` calls the strip binary by path (no `cargo run`); comment updated.
- `crates/cli/src/e2e/tracer.rs`: new `strip_tool(work)` copies `current_exe()` to `work/cli-strip`; `Wrangler::start` sets `HUB_STRIP_BIN`.
- `scripts/hub-test.sh`: `cargo build -q -p cli` after worker-build; copy `$SHARED_TARGET_DIR/debug/cli` to `$STATE/cli-strip`; wrangler gets `HUB_STRIP_BIN`.
- `web/tests/helpers-hub.sh`: same build, copy and `HUB_STRIP_BIN` as hub-test.sh.
- `docs/ledger/reports/MVP-000h.md`: this report.

## Verify output

| Command | Result |
| --- | --- |
| `bash scripts/hub-test.sh smoke` | PASS, `ready after ~16s`, `test result: ok. 4 passed; 0 failed`, `PASS hub-test smoke`, EXIT 0 |
| `cargo run -p cli -- e2e tracer` | PASS, `ready after 20.6s`, seq 1..=17 strictly increasing, second answer HTTP 409, `PASS e2e tracer in 79.9s`, EXIT 0 |
| `cargo clippy -p cli --all-targets -- -D warnings` | PASS, `Finished dev profile`, EXIT 0 |
| `cargo fmt --all -- --check` | PASS, EXIT 0 |

## Acceptance criteria

1. **Partly met.**
   - Met: the scripts and the tracer build the strip tool once before wrangler starts, and the `[build]` command calls the binary by path with no nested cargo run. Evidence: hub-test smoke and the tracer both reach Ready and pass with `HUB_STRIP_BIN` set to a copy.
   - Not met (not verified): "wrangler dev reaches Ready while another cargo build holds the target lock". The manual check was planned as a background `cargo build -p client-gpui` holding the shared lock, then a plain `wrangler dev`. The auto-mode classifier denied that command (reason: interferes with workloads), so it was not run. Only the scripted runs above were executed, and they did not hold a lock.
   - Not verified: `web/tests/helpers-hub.sh` (Playwright not run). Its change mirrors hub-test.sh and has not been executed.

## Notes

- Without `HUB_STRIP_BIN`, the `[build]` command runs `${CARGO_TARGET_DIR:-../../target}/debug/cli`. A manual `wrangler dev` needs `cargo build -q -p cli` first. The fallback is a plain path, not a cargo run.
- The first hub-test smoke compiled native test deps (about 5 min) because the test build is separate from the strip step.

## Discovered work

```yaml
- id: MVP-000h-manual-lock-check
  title: "Manual check: wrangler dev reaches Ready while cargo holds the shared target lock"
  status: ready
  notes: >
    Not run here: the auto-mode classifier denied a background `cargo build -p client-gpui` that
    holds the shared debug lock, followed by a manual `wrangler dev`. Run it by hand in a terminal:
    start the long build, then `wrangler dev --local` in crates/hub with HUB_WASM_TARGET_DIR set,
    and record Ready.
  acceptance: wrangler dev logs Ready while the cargo build still runs; record the log line in MVP-000h.md.
- id: MVP-000h-helpers-playwright
  title: "Run the Playwright hub start (web/tests/helpers-hub.sh) once with the strip change"
  status: ready
  notes: "helpers-hub.sh now builds cli and sets HUB_STRIP_BIN; it has not been executed."
  touches: [web/tests/helpers-hub.sh]
```

## blocked_on

None for the code change. The manual lock check needs an operator who can run it by hand.
