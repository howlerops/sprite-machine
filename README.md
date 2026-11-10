# Sprite Machine

An agent development environment: run coding agents (Claude Code, Codex, opencode, Gemini, pi, goose) on your own machine or in the cloud, and follow and steer them from a desktop app, a phone web page or the terminal.

- Plan: [docs/PLAN-v2-ade.md](docs/PLAN-v2-ade.md)
- Work ledger and status: [docs/ledger/STATUS.md](docs/ledger/STATUS.md)
- Developer setup: [docs/dev.md](docs/dev.md), [docs/setup/SETUP.md](docs/setup/SETUP.md)
- Hub operations and deploy: [docs/ops/hub.md](docs/ops/hub.md)
- Desktop builds (unsigned macOS app from CI): [docs/ops/desktop-builds.md](docs/ops/desktop-builds.md)

## Layout

| Path | What |
| --- | --- |
| `crates/hub` | Cloudflare Worker hub (Rust, wasm32): threads, machines, auth, push |
| `crates/runner` | `sm` runner: ACP client, tunnel to the hub, TUI, `sm rc` |
| `crates/cli` | the `sm` binary and end-to-end tests (`cargo run -p cli -- e2e app`) |
| `crates/client-core` | shared client logic: sync, outbox, view models |
| `crates/client-gpui` | desktop app (gpui) |
| `crates/protocol` | wire types and specs' schemas |
| `web/` | mobile web page served by the hub |
| `workers/workspace` | Workspace Durable Object Worker (TypeScript) |
| `docs/specs` | tunnel, client API and turn-event specs |

## Quick checks

```bash
cargo test --workspace
cargo run -p cli -- e2e tracer
bash scripts/hub-test.sh thread_do
```

The v1 TypeScript apps were retired on 2026-11-10; the last v1 commit is on the `v1-final` branch.
