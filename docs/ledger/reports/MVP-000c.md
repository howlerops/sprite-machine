# MVP-000c report: hub wasm size breakdown

## Summary

- The MVP-000a suspect is wrong. The ACP mapper and the `agent-client-protocol-schema` types are not in the hub binary: twiggy shows the schema crate at 31 bytes, and the `acp` and `mapping::acp_turn` mapper symbols at 0. Gating them would save about 0 bytes of wasm.
- Breakdown in `docs/spikes/hub-size.md`. The biggest contributor is the wasm name section ("function names", 678 KB, 33% of the binary). It cannot be removed here: `strip = true` fails in worker-build 0.8.7 (reproduced), and the fix is in the root `[profile.wasm-release]` / worker-build, outside this item's paths.
- Gating `protocol::acp` is blocked: `mapping::acp_turn` (which the hub needs for `SequenceValidator`) imports `crate::acp`, and the validator lives inside `acp_turn.rs`, which is not in the allowed paths. No manifest or source change was made.
- The hub is 2,048,503 bytes raw (579,274 gzip) for the wasm, and 587,905 bytes gzip for the bundle. That is about 4.0x the spike (147,705), and about 43% above MVP-000a's 412,371, because the hub has grown since then.
- All verify commands exit 0 on the unchanged tree. The tracer prints the same bundle size before and after (no change was made).

## Files changed

- `docs/spikes/hub-size.md` (new): before numbers, twiggy top 30, per-crate breakdown, what was tried, discovered work.
- `docs/ledger/reports/MVP-000c.md` (new): this report.

No change to `crates/protocol/Cargo.toml`, `crates/protocol/src/lib.rs`, `crates/protocol/src/mapping/mod.rs`, `crates/hub/Cargo.toml`, or `Cargo.lock`. Build outputs under `crates/hub/build` are gitignored.

## Bundle sizes (before / after)

Measured with `worker-build --profile wasm-release` in `crates/hub` (RUSTFLAGS unset) and the tracer's gzip -9 method. The after numbers are equal to the before numbers because no code changed.

| Item | Before raw | Before gzip -9 | After raw | After gzip -9 |
| --- | ---: | ---: | ---: | ---: |
| `build/index_bg.wasm` | 2,048,503 | 579,274 | 2,048,503 | 579,274 |
| `build/index.js` | 38,257 | 8,631 | 38,257 | 8,631 |
| Bundle (sum of gzip) | 2,086,760 | 587,905 | 2,086,760 | 587,905 |

Reference: spike 147,705 gzip (`docs/spikes/cloudflare.md`); MVP-000a 412,371 gzip (stale: the hub grew).

## Criteria

1. **Met.** `docs/spikes/hub-size.md` has the twiggy top 30 and the dominators output for the wasm, the per-crate breakdown, and the named contributors. Before and after numbers are recorded above. The "after" is the same tree (no change).
2. **Not met as written.** The brief asked to gate `protocol::acp` and `mapping::acp_turn` minus the validator behind a default-on `acp` feature. Reasons, all documented in the spike doc:
   - Measured, the gate saves about 0 bytes. The ACP code is dead-code eliminated already.
   - It cannot be done inside the allowed paths. `mapping::acp_turn` is one file (`acp_turn.rs`) that holds both the ACP mapper (imports `crate::acp`) and the validator's module declaration. The validator (`acp_turn/validate.rs`) itself uses only `crate::turn_event` and std, so the fix is to move it out of `acp_turn.rs` (see discovered work). Moving it was not allowed by this item ("move nothing").
   - No feature was added. The hub would not compile with `acp` off.
   The tracer stays green (criterion 3 below), but the bundle did not shrink.
3. **Partly met.** The tracer stays green and prints the gzip bundle (587,905 bytes). It did not get smaller, because no size reduction was possible within the allowed paths. Recorded above.

## Verify output

```
$ cargo run -p cli -- e2e tracer          # exit 0
index_bg.wasm: 2048503 bytes, gzip -9 579274 bytes
bundle gzip -9: 587905 bytes (574.1 KiB), limit 1048576 bytes
...
stream matches turn-event.md §5 rule 6; seq 1..=15 strictly increasing
second answer to the interrupt: HTTP 409 (first answer wins)
PASS e2e tracer in 38.8s

$ test -s docs/spikes/hub-size.md         # exit 0

$ cargo test --workspace                  # exit 0
test result: ok. ... (all crates; 0 failed; one ignored test pre-existing)

$ env -u RUSTFLAGS cargo build -p hub --target wasm32-unknown-unknown   # exit 0
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 7.79s

$ bash scripts/check-no-tokio-runtime.sh hub   # exit 0
ok: tokio in hub only via worker, no features (no runtime)

$ bash scripts/hub-test.sh thread_do      # exit 0
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.79s
PASS hub-test thread_do

$ cargo fmt --all --check                 # exit 0

$ cargo clippy --workspace --all-targets -- -D warnings   # exit 0
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 11.85s
```

Experiment not committed: `CARGO_PROFILE_WASM_RELEASE_STRIP=symbols worker-build --profile wasm-release` fails with `failed to generate catch wrappers` (wasm-bindgen externref), so the name section cannot be stripped by the profile.

## Discovered work (YAML)

```yaml
- id: MVP-000e
  title: "Hub wasm: move SequenceValidator out of mapping::acp_turn so ACP can be gated"
  why: >
    The ACP schema and mapper contribute 0 bytes to the hub wasm (twiggy), so gating them is
    a dependency-graph and compile-time win, not a size win. The hub needs only
    mapping::acp_turn::SequenceValidator, which lives in acp_turn.rs next to the ACP mapper.
    Move the validator (acp_turn/validate.rs, re-exported) into a module that does not import
    crate::acp, then gate protocol::acp, mapping::acp_turn and agent-client-protocol-schema
    behind a default-on `acp` feature that the hub disables.
  files: [crates/protocol/src/lib.rs, crates/protocol/src/mapping/mod.rs, crates/protocol/src/mapping/acp_turn.rs, crates/protocol/Cargo.toml, crates/hub/Cargo.toml]
  acceptance:
    - "hub builds with default-features = false; sidecar, acp-mock, cli keep the default feature"
    - "cargo tree -p hub --target wasm32-unknown-unknown has no agent-client-protocol-schema"
    - "tracer green; wasm size unchanged within noise"
- id: MVP-000f
  title: "Hub wasm: drop the name section (33% of the binary) without strip"
  why: >
    twiggy: "function names" + "data names" = 692 KB of 2,048,503 bytes raw. `strip = true`
    breaks worker-build 0.8.7 (externref catch wrappers; reproduced in MVP-000c). Find another
    way to remove the names: a wasm-opt pass in worker-build config (for example dropping the
    name section), or a worker-build / wasm-bindgen version that builds with strip. Change is
    in the root [profile.wasm-release] or worker-build config, outside MVP-000c's paths.
  acceptance:
    - "tracer prints a smaller gzip bundle; the hub still builds and thread_do passes"
    - "docs/spikes/hub-size.md after-numbers updated"
```

## Notes

- Commit attribution: the harness reminder asks for `Co-Authored-By: Claude Haiku 5.5`, and the brief asks for `Opus 5.5`. The commit uses the harness line, which is the attribution guidance in effect for this session. Change it if the ledger expects Opus.
- The twiggy "name section" share is an upper bound on what dropping names would save: gzip compresses names well, so the gzip saving will be smaller than 692 KB.
- The spike was not rebuilt here, so the 4.0x ratio uses the spike's recorded 147,705 gzip.
