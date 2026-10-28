# Spike: hub wasm size breakdown (MVP-000c)

Question: why is the hub Worker bundle about 3x the Cloudflare spike (147,705 bytes gzip),
and which parts can be cut? Spike numbers: [cloudflare.md](cloudflare.md).

Hypothesis from MVP-000a: `protocol` pulls in `agent-client-protocol-schema` (ACP types), and
that code is what makes the hub large. **Measured: the hypothesis is wrong.** The ACP mapper
and the ACP schema types are not in the hub binary at all (dead-code eliminated by LTO and
wasm-ld). The bytes are the wasm name section, the hub's own code, and serde and workers-rs
glue. Details below.

## Measurement setup

- Commit: `9d581fe` (branch `worktree-agent-ab0fbc64617a1ae88`, from `v2`). Source of the hub
  and protocol unchanged by this item, so "before" and "after" are the same tree.
- Profile: `[profile.wasm-release]` (root `Cargo.toml`: `opt-level = "z"`, `lto = true`,
  `codegen-units = 1`, `strip = false`), via `worker-build --profile wasm-release` from
  `crates/hub`, RUSTFLAGS unset (`env -u RUSTFLAGS`).
- Tools: twiggy 0.8.0 (`cargo binstall twiggy`), gzip -9 (same as the tracer's `gzip_size`).
- Commands:

  ```
  cd crates/hub && worker-build --profile wasm-release
  twiggy top -n 30 build/index_bg.wasm
  twiggy dominators -d 3 -r 40 build/index_bg.wasm
  cargo run -p cli -- e2e tracer     # prints the gzip bundle
  ```

## Before (this tree, measured 2026-10-28)

| Item | Raw bytes | gzip -9 bytes |
| --- | ---: | ---: |
| `build/index_bg.wasm` | 2,048,503 | 579,274 |
| `build/index.js` | 38,257 | 8,631 |
| **Bundle (index.js + index_bg.wasm)** | 2,086,760 | **587,905** |

Ratio to the spike (147,705 gzip): **about 4.0x**. The spike's raw wasm was 487,908 bytes; the
hub is 4.2x that.

Note on MVP-000a: its report measured 1,462,185 raw / 412,371 gzip for the same profile. The hub
has grown since then (about +586 KB raw on the same profile). The 412 KB figure is stale.

## Breakdown: twiggy top 30 (before)

Shallow bytes, `twiggy top -n 30 build/index_bg.wasm`. Long generic names are cut to 150 chars.

```
    Bytes     %     Item
   678438   33.12%  "function names" subsection
    44503    2.17%  js_sys::futures::future_to_promise<TracerHub::websocket_message closure ...>
    43523    2.12%  core::panic::unwind_safe::AssertUnwindSafe<Thread::fetch closure ...>
    24416    1.19%  js_sys::futures::future_to_promise<TracerHub::fetch closure ...>
    20756    1.01%  hub::workspace_do::dev::dev_rpc as axum Handler::call closure
    20684    1.01%  protocol::turn_event::event::Event as Serialize::serialize (serde_json)
    19403    0.95%  data segment ".rodata.2"
    15724    0.77%  serde_json::ser::to_vec<protocol::turn_event::Envelope>
    13927    0.68%  <f64 as core::fmt::Display>::fmt
    13918    0.68%  "data names" subsection
    12442    0.61%  js_sys::futures::future_to_promise<hub::worker::_worker_fetch closure ...>
    11989    0.59%  <http::header::name::StandardHeader>::from_bytes
    11037    0.54%  <url::host::Host<Cow<str>>>::parse_cow
    10984    0.54%  data segment ".rodata.181"
    10265    0.50%  hub::threads::routes::forward as axum Handler::call closure
     9631    0.47%  hub::auth::routes::bootstrap as axum Handler::call closure
     9266    0.45%  data segment ".rodata.440"
     9061    0.44%  protocol::tunnel::types::ContentPart as Deserialize (serde_wasm_bindgen)
     8270    0.40%  data segment ".rodata.882"
     8246    0.40%  protocol::mapping::acp_turn::validate::SequenceValidator::push
     8129    0.40%  hub::threads::core::ThreadCore<SqlStore>::append_inner
     7852    0.38%  axum::routing::path_router::PathRouter::set_node
     6456    0.32%  hub::auth::routes::issue as axum Handler::call closure
     6251    0.31%  hub::threads::core::ThreadCore<MemoryStore>::append_inner
     6230    0.30%  protocol::turn_event::event::RunOutcome as Deserialize
     5918    0.29%  hub::threads::sqlite::from_json<hub::threads::store::TaskRecord>
     5401    0.26%  serde_json::value::Value as Deserializer::deserialize_struct (Envelope)
     5321    0.26%  protocol::tunnel::frame::Frame as Serialize::serialize
     5291    0.26%  serde ContentDeserializer::deserialize_any (Command)
     5236    0.26%  data segment ".rodata.377"
  989935   48.32%  ... and 4849 more rows
 2048503  100.00%  total (4879 rows)
```

## Breakdown by crate (before)

Attribution is by the crate named first in each twiggy item (approximate: generic
instantiations are charged to the crate that names the type). Bytes are shallow.

| Crate (first name in item) | Bytes | Share |
| --- | ---: | ---: |
| (wasm name section, "function names") | 678,438 | 33.1% |
| `protocol` | 179,866 | 8.8% |
| `hub` | 163,576 | 8.0% |
| `core` (monomorphised generics, fmt, iter) | 152,360 | 7.4% |
| `js_sys` (future_to_promise glue) | 125,238 | 6.1% |
| `serde` | 118,401 | 5.8% |
| `serde_json` | 95,502 | 4.7% |
| `alloc` | 55,129 | 2.7% |
| `axum` | 37,179 | 1.8% |
| `url` | 30,522 | 1.5% |
| `http` | 29,084 | 1.4% |
| `worker` | 25,595 | 1.2% |
| `serde_wasm_bindgen` | 15,557 | 0.8% |
| data segments and other sections | the rest | |

Items under `protocol`, by module:

Rows whose symbol names mention each module (overlapping; includes generic instantiations
that the hub's own code causes, so these are upper bounds):

| Module | Bytes (rows mentioning it) | Used by hub? |
| --- | ---: | --- |
| `turn_event` (Event/Envelope serde, ids, time) | 225,079 | yes |
| `tunnel` (frames, commands, types) | 116,368 | yes |
| `mapping::acp_turn::validate` (SequenceValidator) | 11,418 | yes (`threads/core.rs`) |
| `mapping::acp_turn` mapper, `convert`, `state` | **0 bytes** (not in binary) | no |
| `acp` (re-export of `agent-client-protocol-schema`) | **0 bytes** (one 31-byte generic) | no |
| `a2a`, `agui` | 0 (modules are empty stubs) | no |

## Dominator view (before)

`twiggy dominators -d 3 -r 40` shows the largest retained trees are the wasm function table
(`table[0]`, 55.8%, not a useful split) and the `future_to_promise` wrappers for
`Thread::fetch` (143,118 bytes retained, 7.0%) and `TracerHub::websocket_message` (79,827,
3.9%). These are the Durable Object and Worker entry points; their retained size is the
handler code they pull in (axum routing, serde, the tunnel and turn-event types), not any one
dependency.

## What was tried

1. **Gate the ACP schema out of wasm32 (the MVP-000a suspect).** Not done, and not worth doing
   for size. Twiggy shows the schema crate contributes 31 bytes to the hub binary. LTO and
   wasm-ld already remove the unreferenced ACP mapper and schema code. Gating `protocol::acp`
   would still not compile the hub, because `protocol::mapping::acp_turn` (which the hub needs
   for `SequenceValidator`) imports `crate::acp` at `acp_turn.rs:32`. The validator itself
   (`acp_turn/validate.rs`) uses only `crate::turn_event` and std; it is only reachable through
   the `acp_turn` module file, which is not in this item's allowed paths. Gating ACP for the
   dependency graph (compile time, and `serde_with`/`strum`/`derive_more`/`convert_case`
   pulled in by the schema crate) needs `validate` moved out of `acp_turn.rs` first. See
   discovered work.
2. **`strip = true` to drop the name section (33%).** Fails in worker-build 0.8.7 with
   "failed to generate catch wrappers" (wasm-bindgen externref), as
   [cloudflare.md](cloudflare.md) records. Reproduced here with
   `CARGO_PROFILE_WASM_RELEASE_STRIP=symbols`. The fix belongs in the root profile or in
   worker-build config, outside this item's allowed paths.
3. **Drop unused `worker` / `axum` features in `crates/hub/Cargo.toml`.** No candidate: `queue`
   (`#[event(queue)]` in `hub/src/worker.rs`), `d1`, `http`, `axum`, and axum `json` are all
   used.

## After

No source or manifest change was made under this item, so the after numbers equal the before
numbers. Re-measure after the discovered work lands.

| Item | Before (raw / gzip) | After (raw / gzip) |
| --- | ---: | ---: |
| `build/index_bg.wasm` | 2,048,503 / 579,274 | 2,048,503 / 579,274 |
| `build/index.js` | 38,257 / 8,631 | 38,257 / 8,631 |
| Bundle gzip (tracer) | | 587,905 |

## Biggest contributors, in order

1. The wasm name section (`function names` + `data names`): 692 KB raw, 34% of the binary.
   Removable only with `strip` (blocked by worker-build 0.8.7) or by a wasm-opt pass that drops
   names.
2. Hub code (routes, Thread/TracerHub handlers, store): 164 KB, plus the monomorphised
   `future_to_promise` glue (`js_sys`, 125 KB).
3. `protocol` (turn_event and tunnel serde impls): 180 KB first-crate attribution. The ACP code is
   not part of this (0 bytes).
4. serde and serde_json monomorphised for the protocol types: about 215 KB combined.
5. axum, http, url, worker glue: about 120 KB combined.

## Discovered work

- Move `SequenceValidator`, `Rule`, `SequenceError`, `validate_sequence` and `validate_complete`
  out of `mapping/acp_turn.rs` into a module that does not import `crate::acp` (e.g.
  `mapping/validate.rs`, re-exported from `acp_turn` for compatibility). Then gate `protocol::acp`,
  `mapping::acp_turn` and the optional `agent-client-protocol-schema` dependency behind a default-on
  `acp` feature that the hub disables. Expected wasm size change: about 0 bytes; expected gain:
  smaller hub dependency graph and compile time.
- Root `Cargo.toml` `[profile.wasm-release]`: find a way to drop the name section without
  `strip = true` (for example a wasm-opt pass in worker-build config, or a worker-build version
  that fixes the externref catch-wrapper error). Expected gain: about 690 KB raw, about 1/3 of the
  binary. This is the largest single lever found.

## After MVP-000f (2026-10-28)

The `name` and `producers` custom sections are dropped after `worker-build` by
`cli wasm strip-names` (crates/cli/src/wasm.rs). The step is the second command of the
`[build]` command in `crates/hub/wrangler.toml`; the tracer runs the same function after
`worker-build`. Every other section is copied byte for byte, so code offsets do not change.
`strip = true` is still not used.

Measured with `cargo run -p cli -- e2e tracer`, on this tree, in the same run conditions as the
"Before" section (the tree is the commit before this change, `ccb4417`, for "before"):

| Item | Before raw / gzip -9 | After raw / gzip -9 |
| --- | ---: | ---: |
| `build/index_bg.wasm` | 2,357,973 / 663,536 | 1,543,450 / 576,194 |
| `build/index.js` | 38,098 / 8,948 | 38,098 / 8,948 |
| **Bundle (tracer's gzip sum)** | | **672,484 before, 585,142 after** |

- Wasm raw size: -814,523 bytes (-34.5%). The name section is about a third of the raw binary,
  as twiggy showed.
- Bundle gzip: -87,342 bytes (-13.0%). The name section compresses well (about 7.6:1 with gzip
  -9), so it is only about 87 KB of the gzip bundle.
- **Target not met:** the item asked for at least 25% smaller gzip bundle (at most 504,363 bytes).
  Dropping the name section alone cannot reach it; the rest of the gzip bundle is hub, protocol,
  serde, axum and workers-rs code, which this item may not change. See the MVP-000f report for
  the blocked_on note.
