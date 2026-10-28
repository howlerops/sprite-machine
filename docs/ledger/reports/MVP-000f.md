# MVP-000f report: strip the hub wasm name section

BRANCH and SHA: see the worker's final reply header.

## Summary

- Added `cli wasm strip-names <file.wasm>` (crates/cli/src/wasm.rs): a hand-written section walker that drops the `name` and `producers` custom sections. No new crates, no wasm-opt.
- The step runs after `worker-build` in the wrangler `[build]` command and in the tracer. Criterion 1 is met, with 7 unit tests for the walker.
- Measured on this tree: bundle gzip 672,484 before, 585,142 after (-12.99%). The wasm itself is -34.5% raw.
- **Criterion 2 is not met:** the 25% gzip target (at most 504,363 bytes) is unreachable by dropping the name section. Blocked on a decision, see below.
- Tracer, `hub-test thread_do`, fmt, clippy and `cli` tests all pass.

## Files changed

- `crates/cli/src/wasm.rs` (new): the stripper, `run` for `wasm strip-names`, unit tests.
- `crates/cli/src/main.rs`: `mod wasm;`, dispatch for `wasm`, usage line.
- `crates/cli/src/e2e/tracer.rs`: runs `wasm::strip_file` after `worker-build`, prints the before and after sizes.
- `crates/hub/wrangler.toml`: `[build] command` adds `&& cargo run -q -p cli -- wasm strip-names build/index_bg.wasm`.
- `Cargo.toml` (root): comment on `[profile.wasm-release]` only. No profile or dependency change.
- `docs/spikes/hub-size.md`: dated "After MVP-000f (2026-10-28)" section with the numbers.
- `docs/ledger/reports/MVP-000f.md` (this file).
- `Cargo.lock`: unchanged (no new crates).

## Verify output

Each command was run after `touch` of the source files, with the shared target dir and the environment from the item.

1. `cargo test -p cli`: PASS. `test result: ok. 14 passed; 0 failed`
2. `cargo run -p cli -- e2e tracer`: PASS (second run; first run failed, see Notes).
   ```
   strip-names: index_bg.wasm 2357973 -> 1543450 bytes
   bundle gzip -9: 585142 bytes (571.4 KiB), limit 1048576 bytes
   stream matches turn-event.md §5 rule 6; seq 1..=17 strictly increasing
   second answer to the interrupt: HTTP 409 (first answer wins)
   PASS e2e tracer in 107.9s
   ```
3. `bash scripts/hub-test.sh thread_do`: PASS. `test result: ok. 7 passed; 0 failed` then `PASS hub-test thread_do`
4. `cargo fmt --all --check`: PASS (exit 0, no output)
5. `cargo clippy --workspace --all-targets -- -D warnings`: PASS. `Finished dev profile ... in 9.12s`

## Acceptance criteria

1. **Met.** After `worker-build`, the wrangler `[build]` command and the tracer both run `wasm strip-names` on `build/index_bg.wasm`. The walker drops custom sections `name` and `producers` and copies all other sections byte for byte, including id and size. Code offsets are unchanged because no index refers to a custom section. Unit tests (`crates/cli/src/wasm.rs`, `cargo test -p cli`, 14 pass):
   - `name_and_producers_sections_are_removed`: a module with a name section and a producers section loses both, and the rest equals the expected bytes.
   - `other_sections_are_byte_identical`: kept sections, including a custom section named `name2` and a code section containing name-like bytes, are unchanged.
   - `bad_magic_is_an_error`, `section_past_the_end_is_an_error`, `truncated_size_is_an_error`, `non_utf8_custom_name_is_an_error`, `custom_name_longer_than_section_is_an_error`: invalid input is an error.
   - Also: `module_without_names_is_unchanged`, `stripping_twice_is_the_same_as_once`, `unknown_subcommand_is_a_usage_error`.
   The step was also run by hand (`cargo run -q -p cli -- wasm strip-names build/index_bg.wasm` after `worker-build`) and inside a manual `wrangler dev --local` run, which printed `Ready`. No wasm-opt or other download was added.

2. **Not met (blocked).** Recorded measurements, same tree, same tracer:

   | Bundle (`index.js` + `index_bg.wasm`) | Raw bytes | gzip -9 bytes (tracer) |
   | --- | ---: | ---: |
   | Before (`index_bg.wasm` 2,357,973 + `index.js` 38,098) | 2,396,071 | **672,484** |
   | After (`index_bg.wasm` 1,543,450 + `index.js` 38,098) | 1,581,548 | **585,142** |

   Reduction: 87,342 bytes, **12.99%**. The target was at least 25% (at most 504,363 bytes). The name section is about 34.5% of the raw wasm, but gzip compresses it about 7.6:1, so it is only 87,342 bytes of the gzip bundle. The rest is hub, protocol, serde, axum and workers-rs code, and this item may not change it.

   Other criteria in this item are met: the hub still builds through the wrangler `[build]` command (checked by hand with `wrangler dev --local`, which reached Ready), `hub-test thread_do` and the tracer pass, and `docs/spikes/hub-size.md` has the dated "after" section.

## Notes

- The first tracer run failed with `wrangler dev not ready after 240s`. The wrangler build step's nested `cargo run -p cli` waited on the shared target dir's cargo lock, which other workers held (`worker-build --release` and hub builds were running). The second run passed. A nested `cargo run` on its own does not deadlock (checked with a scratch project). The cause is shared-target-dir contention, not a bug in the stripper, but it can recur.
- `cargo fmt --all` reformatted `wasm.rs` once before the tests.

## Discovered work

```yaml
- id: MVP-000g
  title: Hub bundle: reach a 25% gzip cut (name section alone gives 13%)
  status: blocked_on_human
  blocked_on: >
    Decide the target. Removing the name section gives 12.99% gzip (672,484 to 585,142 bytes).
    A 25% cut needs at most 504,363 bytes, so about 81 KB more gzip must go from the hub, protocol,
    serde/serde_json and js_sys glue. Options: drop the name section only if the 25% target is
    replaced by a raw-size target; or cut the monomorphised future_to_promise and serde glue (see
    docs/spikes/hub-size.md, the per-crate table).
  acceptance: the chosen gzip target is met on `cargo run -p cli -- e2e tracer`, with numbers recorded.

- id: MVP-000h
  title: Make the wasm strip step independent of the shared cargo target lock
  status: ready
  notes: >
    The wrangler [build] command runs `cargo run -p cli -- wasm strip-names`. With the shared
    CARGO_TARGET_DIR, it waited on the cargo build lock held by other workers and wrangler dev
    timed out at 240s once. Options: a separate small binary target built once, or a fixed
    checkout-relative path to the cli binary that the tracer builds.
  acceptance: wrangler dev reaches Ready while another cargo build holds the shared target dir lock.
```

## Hand-off

Criterion 2 is not met. The stripper and its tests are in place and can be merged. The 25% gzip target needs a decision (see MVP-000g above).
