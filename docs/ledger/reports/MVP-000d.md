# MVP-000d: tracer onto the real /v1 routes

## Summary
- `cli e2e tracer` now registers a machine (`POST /v1/machines`, `Bearer dev`), creates the thread with `POST /v1/threads` (Idempotency-Key), subscribes on `GET /v1/threads/{id}/subscribe` and answers on `POST /v1/threads/{id}/interrupts/{id}` (second answer 409). The runner uses the Machine DO tunnel. D1 migrations are applied before wrangler starts.
- The hub emits `UserMessage`; the bridge no longer does for `prompt`/`dispatch`. `Config::emit_user_message` is removed. A prompt whose idempotency key starts with `LOCAL_KEY_PREFIX` ("local:") is a locally typed prompt and still emits it.
- `crates/hub/src/tracer`, the tracer routes, `HostKind::Tracer`, the `HUB` binding and the `TracerHub` class are gone; wrangler.toml migration `v3` has `deleted_classes = ["TracerHub"]`.

## Files changed
- Deleted: `crates/hub/src/tracer/{mod,state,thread,worker}.rs`
- `crates/hub/src/{lib.rs,worker.rs}`, `crates/hub/wrangler.toml`
- `crates/hub/src/machines/{store,internal,driver,core,routes,tests}.rs` (HostKind::Tracer, TRACER_* consts, tracer guards and the tracer presence test removed)
- `crates/client-core/src/tracer.rs` (create via POST /v1/threads, base `…/v1`, dev token, one event loop)
- `crates/cli/src/e2e/tracer.rs` (D1 migrations, machine creation, `/healthz` probe, stream check ignores hub `StateSnapshot`/`StateDelta`/`Presence`)
- `crates/sidecar/src/bridge/{mod,io,runtime}.rs`, `crates/sidecar/src/tracer.rs`, `crates/sidecar/tests/{bridge,tracer_bridge}.rs`
- `docs/ledger/reports/MVP-000d.md`

## Verify output (all exit 0)
- `cargo run -p cli -- e2e tracer`: PASS. Tail: `stream matches turn-event.md §5 rule 6; seq 1..=17 strictly increasing` / `second answer to the interrupt: HTTP 409 (first answer wins)` / `PASS e2e tracer in 60.5s`. Bundle gzip 635 KiB.
- `test ! -d crates/hub/src/tracer`: pass.
- `cargo test -p sidecar`, `-p hub`, `-p client-core`, `-p runner`: pass.
- `bash scripts/hub-test.sh` machine_do, thread_do (`PASS hub-test thread_do`), smoke, plus thread_create, authz, attachments: all pass.
- `env -u RUSTFLAGS cargo build -p hub --target wasm32-unknown-unknown` and `bash scripts/check-no-tokio-runtime.sh hub`: pass.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`: pass.

## Acceptance criteria
1. Met. The e2e run above (events: StateSnapshot, UserMessage, Presence, then the rule 6 shape; the three hub state/presence events are filtered from the shape check but seq continuity covers all 17). `grep -ri tracerhub crates` finds only the wrangler.toml v1/v3 migration entries; no unprefixed routes, no HostKind::Tracer; wrangler.toml has no HUB binding and migration `v3` deletes TracerHub.
2. Met. Hub-issued prompts: `tracer_bridge::prompt_permission_resume_end_turn` (no UserMessage, stamps 1..=13) and the existing bridge tests (first event is RunStarted). Local prompt: `bridge::a_prompt_typed_locally_makes_the_bridge_emit_user_message` (`CommandFeed::send_local`, key `local:N`).
3. Met (see verify).

## Discovered work
```yaml
- id: MVP-000e
  title: Hub refuses an Idempotency-Key starting with "local:" on prompt/create
  why: The bridge treats a prompt command whose key starts with "local:" (LOCAL_KEY_PREFIX) as typed locally and emits UserMessage. valid_idempotency_key allows ':' so a client could send such a key and get a duplicate UserMessage.
  files: crates/hub/src/threads/**
- id: MVP-000f
  title: Real local-prompt path in the runner (TUI//rc) uses LOCAL_KEY_PREFIX
  why: Nothing in the runner yet feeds a locally typed prompt to the bridge; when the TUI does, it must send keys prefixed LOCAL_KEY_PREFIX (or the contract becomes an explicit IncomingCommand flag, touching crates/runner/src/tunnel/**).
- id: MVP-000g
  title: Tidy stale tracer wording outside this item's file list
  why: crates/hub/src/do/machine.rs (header comment, 400 message "host is thread or tracer"), crates/hub/src/threads/{mod,core,store}.rs comments, crates/client-core/src/outbox.rs doc on the tracer-only machine_id/author/resolved_by fields still mention the tracer hub.
```
