# Spike: Cloudflare Workers in Rust (M0-017)

Code: [`spikes/cf`](../../spikes/cf). Run: `bash spikes/cf/test.sh`.
Plan context: [PLAN-v2-ade.md §4.2](../PLAN-v2-ade.md) ("Limits to design around") and §7.2.

## What the spike shows

A standalone Cargo project (own empty `[workspace]`, own `Cargo.lock`) built with
`worker-build --release` and served by `wrangler dev --local`. All of it works locally with
placeholder binding ids and no Cloudflare account.

| Route | What it proves |
| --- | --- |
| `GET /health` | `#[event(fetch)]` hands an `http::Request` to an axum 0.8 `Router` (`http` + `axum` features). |
| `GET/POST /counter/{name}` | Request forwarded unchanged to the `Room` Durable Object. SQLite storage (`new_sqlite_classes`), schema made in the constructor, `INSERT … ON CONFLICT … RETURNING` upsert. One database per name; the value survives across requests. |
| `GET /ws/{name}` | WebSocket upgrade forwarded through axum to the DO. The DO accepts it with the **hibernation API** (`state.accept_web_socket`), echoes text in `websocket_message`, and answers the literal `ping` with `pong` through **`set_websocket_auto_response`** (the reply proves the runtime answered: `websocket_message` would have sent `echo:ping`). A plain GET gets 426. |
| `GET/POST /d1/notes` | D1 binding: `prepare().bind().first()` with `RETURNING`, `all().results::<T>()`. Schema from `migrations/0001_notes.sql`, applied by `wrangler d1 migrations apply DB --local`. |
| `PUT/GET /r2/{key}` | R2 binding: `put().execute()`, `get().execute()` then `body().bytes()`; a missing key gives 404. |

`test.sh` checks the dependency tree, builds, applies the D1 migration into a temporary
`--persist-to` directory (so every run starts empty), starts `wrangler dev --local` on a free
port in its own process group (`setsid`), asserts status + exact body for every route, runs
`ws-check.mjs` (Node 22 global `WebSocket`), and kills the process group from an `EXIT` trap.
Any mismatch exits non-zero and prints the tail of the wrangler log.

## Measurements

Machine: the 4 vCPU cloud container, `CARGO_BUILD_JOBS=2`, rustc 1.97.0, worker 0.8.7,
worker-build 0.8.7 (wasm-bindgen 0.2.129, wasm-opt 132, esbuild 0.28.2), wrangler 4.154.0.
Release profile: `opt-level = "z"`, `lto = true`, `codegen-units = 1`.

| Item | Value |
| --- | --- |
| `build/index_bg.wasm` | 487,908 bytes |
| `build/index.js` (glue + shim) | 29,613 bytes |
| **Bundle, gzip -9** (index.js + index_bg.wasm) | **147,705 bytes (~144 KiB)** |
| Cold build (empty target dir, crates already downloaded) | 52 s (cargo 49.5 s, wasm-bindgen + wasm-opt + esbuild ~2 s) |
| Incremental build (touch `src/lib.rs`) | 7 s |
| No-op build | 2 s |
| `wrangler dev --local` ready (reruns `[build]`, warm) | ~5 s |
| First-ever `worker-build` run | downloads wasm-bindgen, wasm-opt and esbuild to `~/.cache/worker-build` (needs network once) |
| Crates in the wasm32 normal tree | 121 |

`panic = "abort"` builds but makes the wasm 580 bytes larger, so it buys nothing.

## Limits that matter

Read on 2026-10-27. Numbers are quoted from the linked pages.

| Limit | Value | Source |
| --- | --- | --- |
| Worker bundle size | 64 MiB uncompressed, Free and Paid; "There is no compressed size limit. Only the uncompressed bundle size counts." | <https://developers.cloudflare.com/workers/platform/limits/> |
| Worker startup time | 1 s (wasm instantiation counts) | same |
| Worker memory | 128 MB per isolate, JS heap + wasm | same |
| CPU time | Free 10 ms per HTTP request; Paid 30 s default, up to 5 min | same |
| Subrequests | Free 50, Paid 10,000 per invocation | same |
| Request body | 100 MB (Free/Pro), 200 MB (Business) | same |
| DO storage (SQLite) | 10 GB per object; account unlimited on Paid, 5 GB on Free | <https://developers.cloudflare.com/durable-objects/platform/limits/> |
| DO SQL | row/string/BLOB 2 MB; 100 columns; statement 100 KB; 100 bound params | same |
| DO CPU | 30 s per request/event by default, configurable to 5 min | same |
| **WebSocket message size** | **32 MiB, received messages** | same |
| Hibernatable sockets | 32,768 per DO; ≤10 tags of ≤256 chars each | <https://developers.cloudflare.com/durable-objects/api/state/> |
| Auto-response pair | request and response ≤2,048 chars each; "returned without waking WebSockets in hibernation" | same |
| WebSocket attachment | `serializeAttachment` ≤16,384 bytes | <https://developers.cloudflare.com/durable-objects/best-practices/websockets/> |
| **D1 database size** | 10 GB (Paid) / 500 MB (Free); 50,000 DBs per account (Paid) / 10 (Free); 1 TB account storage (Paid) | <https://developers.cloudflare.com/d1/platform/limits/> |
| D1 queries | 1000 per Worker invocation (Paid) / 50 (Free); 30 s max query; 2 MB row; 100 bound params | same |
| R2 | 5 TiB per object; 5 GiB single-part; 1,024-byte keys; uploads through a Worker capped by the request body limit | <https://developers.cloudflare.com/r2/platform/limits/> |

What this means for the hub:

- **Bundle**: 144 KiB gzip for axum + DO + D1 + R2 leaves a lot of room. The binding
  constraint is the 1 s startup and cold-start latency, not the size cap. Keep a budget
  (test.sh fails above 1 MiB gzip) and watch it as `protocol` and serde types grow.
- **WebSocket**: the 32 MiB platform cap is far above our 1 MiB frame cap (tunnel spec §2),
  so the spec's cap holds. Enforce it in `websocket_message` ourselves.
- **DO storage**: 10 GB per Thread DO is plenty for one thread's event log; the 2 MB row
  limit means large tool outputs go to R2 with a key in the row.
- **D1**: 10 GB per database. One org-wide D1 is fine for P2; plan §4.2's "shard per org if
  needed" stays an option, since the account allows 50,000 databases.
- **CPU**: Free plan (10 ms) is not viable for the hub; deploy on Workers Paid.
- **Hibernation**: auto-response does not wake the DO, which is what tunnel spec §2 needs.
  Alarms, open outbound WebSockets and timers prevent hibernation, so the Machine DO must not
  hold client sockets or `setTimeout`.

## workers-rs gaps and caveats (worker 0.8.7)

1. **tokio is always in the tree.** `worker` depends on `tokio = { default-features = false }`
   unconditionally (it uses only `tokio::io::{AsyncRead, AsyncWrite}` for the TCP `Socket`). So
   `cargo tree --target wasm32-unknown-unknown -e normal` *does* list `tokio v1.54.1`, with no
   features: no runtime, no `rt`/`net`/`time`/`sync`/`macros`. Same on workers-rs `main`
   (2026-10-27). The "no tokio" rule can only mean "no tokio runtime and nothing that needs
   one"; test.sh checks exactly that (tokio reached only via `worker`, feature list empty).
2. **`strip = true` breaks the build.** wasm-bindgen 0.2.129 (run by worker-build 0.8.7 with
   `--force-enable-abort-handler`) fails with "externref table required for catch wrappers".
   Leave `strip` off; wasm-opt removes debug info anyway.
3. **D1 is still behind the `d1` feature** and the README still calls it alpha. The API
   worked for everything we need here (`prepare`, `bind`, `first`, `all`, `results`,
   `batch`, sessions). `bind` takes `&[JsValue]`; `query!` / `bind_refs` give typed binds.
4. **Hibernation API coverage**: `accept_web_socket`, `accept_websocket_with_tags`,
   `get_websockets(_with_tag)`, `get_tags`, `set/get_websocket_auto_response`,
   `serialize/deserialize_attachment` and the `websocket_message/close/error` handlers are
   exposed. **Not exposed**: `getWebSocketAutoResponseTimestamp` (useful for presence: "last
   heartbeat") and `setHibernatableWebSocketEventTimeout`. Both need a small `wasm-bindgen`
   extern on `DurableObjectState` until upstream adds them.
   `set_websocket_auto_response` / `get_websocket_auto_response` `unwrap()` the JS result
   internally, so a JS exception panics instead of returning `Err`.
5. **DO trait defaults panic.** `websocket_message`, `websocket_close`, `websocket_error` and
   `alarm` default to `unimplemented!()`. Implement all of the ones you can receive.
6. **`#[durable_object]` needs `use worker::wasm_bindgen;`** in the module, or the expansion
   fails with "cannot find module `wasm_bindgen`".
7. **axum integration**: `Env` and every binding are `!Send`. Put `Env` in
   `worker::send::SendWrapper` as router state and mark every handler that awaits a JS future
   with `#[worker::send]`. The router is rebuilt per request (cheap). WebSocket upgrades pass
   through axum fine: `worker::Request::try_from(axum_request)`, `stub.fetch_with_request`,
   then `worker::Response -> http::Response<axum::body::Body>` (`axum` feature) keeps the
   `WebSocket` in the response extensions and the fetch macro puts it back on the JS
   `Response`. axum's own `ws` extractor cannot be used (it needs hyper/tokio).
8. **Hibernation under `wrangler dev`** is not observable from the outside; the spike proves
   the auto-response path and the hibernation handlers, not actual eviction. Real eviction
   timing needs a deployed test later (P2).
9. `compatibility_date = "2026-10-01"` turns on `web_socket_auto_reply_to_close`; the spike
   still calls `close(1000)` in `websocket_close`, which is harmless and keeps older dates
   working.

## Recommendations for the hub crate

- **Pin** `worker = "=0.8.7"` and `worker-macros = "=0.8.7"` (features `http`, `axum`, `d1`;
  macros `http`), `axum = { version = "0.8", default-features = false, features = ["json"] }`,
  `tower-service = "0.3"`. Install `worker-build` 0.8.7 (it pins wasm-bindgen 0.2.129, which
  must match the lockfile's `wasm-bindgen`). wrangler 4.154.0.
- **Keep the hub out of the native workspace build** or gate it to `wasm32`: it is a
  `cdylib` for `wasm32-unknown-unknown` only. Unset `RUSTFLAGS` (the lld flag) for wasm builds.
- **Release profile**: `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `strip = false`.
- **Copy from `spikes/cf/src/lib.rs`**: `router(env)` with `SendWrapper<Env>` state,
  `#[worker::send]` handlers, an `AppError` that maps `worker::Error` to a response, and the
  `to_room` handler that forwards a request (including a WebSocket upgrade) to a DO by name.
- **Copy from `spikes/cf/src/room.rs`**: SQLite schema in the DO constructor
  (`CREATE TABLE IF NOT EXISTS`, `exec` is sync), upserts with `RETURNING`, rows decoded with
  `to_array::<T>()`, `accept_web_socket` + `set_websocket_auto_response` on accept, binary
  frames closed with 1003, `websocket_close` answering 1000.
- **wrangler.toml**: `new_sqlite_classes` in the first `[[migrations]]` (a DO class cannot
  switch from KV to SQLite storage later), D1 `migrations_dir`, placeholder ids for local
  runs, `[build] command = "worker-build --release"`, `main = "build/index.js"`.
- **Tests**: copy `test.sh` (free port, `setsid` + process-group kill in a trap, temporary
  `--persist-to`, `wrangler d1 migrations apply --local`, exact-body assertions) and
  `ws-check.mjs` for WebSocket checks.
- **Add the missing hibernation externs** (`getWebSocketAutoResponseTimestamp`,
  `setHibernatableWebSocketEventTimeout`) in the hub, or upstream them.
- Enforce the 1 MiB tunnel frame cap in the DO; store anything over ~1 MB of a row in R2.
