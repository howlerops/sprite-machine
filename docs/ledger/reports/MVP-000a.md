# MVP-000a report: hub build hygiene

## Summary

- `crates/hub/.gitignore` ignores `/.wrangler` and `/build`. The tracer no longer deletes `crates/hub/.wrangler`, and `git status --porcelain` is empty after `cargo run -p cli -- e2e tracer`.
- The hub is built with a named profile `wasm-release` (inherits `release`, `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `strip = false`). `worker-build --profile wasm-release` uses it in the tracer and in `wrangler.toml` `[build]`. Native `[profile.release]` is unchanged, so other crates are not affected. `CARGO_PROFILE_RELEASE_STRIP` is gone from the tracer.
- The tracer prints the gzip -9 size of `build/index.js` and `build/index_bg.wasm` and fails at 1 MiB or more. Current size is 412,371 bytes.
- The tracer finds `crates/hub` at run time by walking up from the current directory to the workspace `Cargo.toml` with `crates/hub/wrangler.toml`. Three unit tests cover the lookup.

## Files changed

- `Cargo.toml` (root): new `[profile.wasm-release]` section only.
- `crates/hub/.gitignore`: new, `/.wrangler` and `/build`.
- `crates/hub/wrangler.toml`: `[build] command = "worker-build --profile wasm-release"` and comment only.
- `crates/cli/src/e2e/tracer.rs`: `--profile wasm-release` instead of `--release`; `CARGO_PROFILE_RELEASE_STRIP` removed; `.wrangler` deletion removed; `report_bundle` and `gzip_size` (shells out to `gzip`, since no new crate dependency was allowed); `hub_dir()` now calls `find_hub_dir()` from the current directory, not `env!("CARGO_MANIFEST_DIR")`; unit tests in `mod tests`.
- `docs/ledger/reports/MVP-000a.md`: this report.

## Profile choice (criterion 2)

Option (c), a custom profile, was chosen over (a) and (b).

- (a) `[profile.release.package.hub]` would change only the hub, but it cannot set LTO, and the spike needs LTO.
- (b) env vars in the build command would reach every cargo invocation that inherits the env, and they are hard to keep in sync with `wrangler dev`, which reruns the `[build]` command.
- (c) `worker-build --profile <name>` (worker-build 0.8.7 supports it) selects a profile that only the hub build uses. Native release builds never read it.

The `wasm-release` profile is used by both the tracer and `wrangler dev`, so the two builds cannot drift.

## Verify output

```
$ cargo run -p cli -- e2e tracer          # exit 0
== worker-build --profile wasm-release (.../crates/hub)
built in 4.7s
index.js: 35796 bytes, gzip -9 7881 bytes
index_bg.wasm: 1462185 bytes, gzip -9 404490 bytes
bundle gzip -9: 412371 bytes (402.7 KiB), limit 1048576 bytes
== wrangler dev --local on port 33083
ready after 6.5s
wrangler stopped
...
stream matches turn-event.md §5 rule 6; seq 1..=15 strictly increasing
second answer to the interrupt: HTTP 409 (first answer wins)
PASS e2e tracer in 13.0s

$ test -z "$(git status --porcelain)"     # exit 0 after the tracer run, on the committed tree
$ cargo test -p cli                        # exit 0: 4 passed (crate_name_is_set + 3 hub_dir lookup tests)
$ cargo fmt --all --check                  # exit 0
$ cargo clippy --workspace --all-targets -- -D warnings   # exit 0
$ pgrep -x workerd                         # no output, exit 1
```

## Bundle sizes (before / after)

| Item | Before (`release`, `strip` via env) | After (`wasm-release`) |
| --- | --- | --- |
| `build/index_bg.wasm` raw | 1,646,436 bytes | 1,462,185 bytes |
| `build/index_bg.wasm` gzip -9 | 499,210 bytes | 404,490 bytes |
| `build/index.js` gzip -9 | 7,852 bytes | 7,881 bytes |
| Bundle gzip -9 (sum of the two files) | 507,062 bytes | 412,371 bytes |

The spike measured 487,908 bytes raw and 147,705 bytes gzip. The hub is now much larger than the spike hub (see discovered work). The profile cut the gzip bundle by about 19%.

## Criteria

1. **Met.** `.gitignore` ignores `/.wrangler` and `/build`. The tracer does not delete `.wrangler`. `git status --porcelain` is empty after the tracer run on the committed tree, and no `workerd` survives.
2. **Met, with a caveat.** Option (c). `opt-level "z"`, `lto`, `codegen-units 1` and `strip off` are in the `wasm-release` profile. The native `release` profile and other crates are unchanged. `CARGO_PROFILE_RELEASE_STRIP` is removed from the tracer. The caveat: the bundle is 412 KB gzip, not the spike's 148 KB (see discovered work).
3. **Met.** The tracer prints the gzip -9 sizes and fails at 1 MiB or more. Current total is 412,371 bytes.
4. **Met.** `hub_dir()` walks up from `current_dir()` and finds the first directory that has `Cargo.toml` and `crates/hub/wrangler.toml`. There is no `CARGO_MANIFEST_DIR` use. Unit tests: `hub_dir_is_found_from_a_nested_directory`, `hub_dir_is_found_from_the_checkout_root`, `hub_dir_lookup_fails_without_a_hub_above`. The `--hub-dir` option was not added because the cli argument parser (`crates/cli/src/e2e/mod.rs`, `main.rs`) is outside the allowed files. The brief said "or", so the walk-up alone meets it.

## Discovered work (YAML)

```yaml
- id: MVP-000c
  title: "Hub wasm size: keep ACP schema types out of the hub bundle"
  why: >
    With the wasm-release profile the hub is 1,462,185 bytes raw (412 KB gzip), against the
    spike's 487,908 bytes (148 KB gzip). The hub depends on `protocol`, which depends on
    `agent-client-protocol-schema` (protocol/Cargo.toml). That is the likely cause; not measured
    with twiggy. Either gate the ACP modules of `protocol` out of wasm32, or keep the hub on
    the turn-event types only, then re-measure.
  files: [crates/protocol/Cargo.toml, crates/protocol/src/lib.rs, crates/hub/Cargo.toml]
  acceptance:
    - "twiggy (or a size diff) shows where the bytes go, documented in this report"
    - "hub gzip bundle reported by `cli e2e tracer` and recorded in the report"
- id: MVP-000d
  title: "Tracer: CI-friendly bundle budget from measured size"
  why: >
    The 1 MiB gate is the MVP-000a acceptance limit. Once MVP-000c lands, a tighter budget
    (for example 200 KB gzip) would catch growth earlier.
  acceptance: ["tracer fails when gzip bundle exceeds the agreed budget"]
```

## Notes

- Commit attribution: the harness reminder asked for `Co-Authored-By: Claude Haiku 5.5`, and the brief asked for `Opus 5.5`. The commit uses the harness line, which names the model that did the work.
- `crates/hub/.wrangler` still exists in the checkout after the tracer run. It is ignored by `crates/hub/.gitignore`, so `git status` stays clean.
