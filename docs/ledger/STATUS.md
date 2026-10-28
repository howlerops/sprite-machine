# Ledger status

_Rewritten by the leader on every ledger commit._

done 51/211 (3 dropped) · in-flight M1A-014a,M1B-004d,M2B-004 · blocked 8 (human: M0-014, M0-020, M1B-001, M1B-010, M2B-020, M3-016, M6-002, M6-006) · attempts 49/120

## Per phase

| Phase | Items | Done | In flight |
| ----- | ----- | ---- | --------- |
| P0 | 12 | 12 | 0 |
| P1 | 38 | 33 | 3 |
| P2 | 31 | 2 | 0 |
| P3 | 20 | 1 | 0 |
| P4 | 33 | 0 | 0 |
| P5 | 30 | 0 | 0 |
| P6 | 28 | 0 | 0 |
| P7 | 16 | 0 | 0 |

## P1 items

| Item | Owner | Lane | Status | Title |
| ---- | ----- | ---- | ------ | ----- |
| MVP-000a | worker | rust | done | Hub build hygiene: crates/hub/.gitignore + a size-optimised wasm release profile |
| MVP-000c | worker | rust | done | Hub wasm size: find and cut what makes the hub bundle 3x the spike (412 KB vs 148 KB gzip) |
| MVP-000b | worker | rust | done | CI: run the tracer bullet (`cargo run -p cli -- e2e tracer`) on every push/PR to v2 |
| SPEC-001a | leader | docs | done | Spec: settle the open points found by the ACP→TurnEvent mapper (turn-event.md) |
| SPEC-003 | leader | docs | done | Spec: client HTTP API (threads, prompts, interrupt answers, cancel, pending tasks) + who emits UserMessage |
| M0-008a | worker | ts | done | workers/workspace: TypeScript Workspace DO skeleton (@cloudflare/sandbox 1.0), wrangler config, typed RPC surface, vitest |
| M0-008b | worker | rust | done | hub ↔ Workspace DO binding from Rust (DO namespace fetch/RPC), with a fake for native tests |
| M1A-001 | worker | rust | done | sidecar (runner-core lib): config, tracing, /healthz server fn |
| M1A-003 | worker | rust | done | sidecar (runner core): ACP client + session table (concurrency, pending tasks) |
| M1A-004 | worker | rust | done | sidecar (runner core): ACP client-side fs + terminals (tee to Activity{terminal}) |
| M1A-006a | worker | rust | done | protocol::mapping ACP→TurnEvent + golden tests |
| M1A-006b | worker | rust | done | runner bridge: ACP client → mapping → TurnEvents to the tunnel client (EventSink) |
| M1A-007 | worker | rust | done | Permission / elicitation ⇄ Interrupt, cancel races (TurnEvent terms) |
| SPEC-002b | leader | docs | done | Spec: tunnel session_config command + client-api POST /v1/threads/{id}/session |
| SPEC-002a | leader | docs | done | Spec: tunnel approval command carries every interrupt outcome (selected, submitted+data, declined, cancelled) |
| M1A-009 | worker | rust | done | Durable store (SQLite) + ACP session re-attach after restart |
| M1A-013 | worker | rust | done | Session config: commands, modes, config options, pending tasks (StateDelta{session}) |
| M1A-014 | worker | rust | done | Attachments: user FilePart → ACP image/resource blocks |
| M1A-014a | worker | rust | assigned | Attachments follow-ups: bridge-level test that a failed fetch never submits; sniff image bytes vs declared MIME |
| M1B-004b | worker | client | done | gpui spike: diff pane (gpui-kit editor) |
| M1B-004c | worker | client | done | gpui spike: terminal pane (alacritty_terminal) |
| M1B-004d | worker | client | review | gpui spike: approval card + build release |
| M1B-008 | worker | docs | done | Benchmark scripts per gates.yaml |
| M1B-008a | worker | client | done | gpui spike: bench hooks (fixture feed, BENCH_MARKER, frame and key-to-glyph JSONL) |
| M1B-008b | worker | client | todo | gpui spike: common workload (5k-line diff, terminal output, one approval card) alongside the streaming transcript |
| M1B-008d | worker | docs | todo | scripts/setup-dev.sh: install xdotool (run-desktop.sh input driver) |
| M2-001 | worker | rust | done | hub Worker skeleton (workers-rs + axum http feature), routing, wrangler.toml with DO/D1/R2/Queue bindings |
| M2-016 | worker | rust | done | hub: D1 schema + migrations (org-wide metadata) |
| M2-003a | worker | rust | done | hub: session tokens, auth middleware, roles + audit log in D1, bootstrap owner |
| M2-008 | worker | rust | done | hub: Thread Durable Object (event log, subscriptions, interrupts, pending tasks, cancel) |
| M2B-001 | worker | rust | done | protocol: tunnel frame types per SPEC-002 (types + tests only) |
| M2B-001a | worker | rust | done | protocol: strict tunnel frame parsing + unknown command kind helper |
| M2B-002 | worker | rust | done | hub: Machine Durable Object (tunnel endpoint, presence, offline queue, ack) + agents.kind migration |
| M2B-003 | worker | rust | done | sidecar: HostMode trait (local | sandbox); sidecar is the runner-core library |
| M2B-004 | worker | rust | assigned | runner: tunnel client with reconnect/backoff/resume |
| M2B-011 | worker | rust | done | runner: local checkpoints as shadow git commits |
| M3-001 | worker | rust | done | client-core: TurnEvent subscription client, view models, cache, outbound queue |
| M3-001a | worker | rust | done | client-core: cache raw envelope JSON so unknown event types survive a restart; outbox failures drive the connectivity banner |

## Waiting on the human

| Item | Phase | Needed |
| ---- | ----- | ------ |
| M0-014 | P6 | Add TS_AUTHKEY (ephemeral, pre-authorized Tailscale key) and WG_TEST_PEER (a WireGuard peer config with a public hostname endpoint) as environment secrets; restart the session. |
| M0-020 | P4 | Cloudflare dashboard → Secrets Store (account level) → create a store if there is none → Create secret: a name such as ANTHROPIC_API_KEY, the value = your model API key (e.g. Anthropic), scope Workers. Tell the leader only the secret name and the store id. Never paste the key into chat. The Workspace DO outbound handler injects it for api.anthropic.com (plan §4.3); it never enters a sandbox. |
| M1B-001 | P7 | Provide an Apple Developer account, a physical iPhone, a mid-range Android phone, and a Mac or approval for GitHub macOS runners (iOS cannot build in the Linux container). |
| M1B-010 | P7 | Build both spikes on a Mac per their READMEs; run spikes/bench on the iPhone and Android; commit results to spikes/bench/results/ with screen recordings for hard gates. |
| M2B-020 | P2 | After the hub is deployed (M2-015): install `sm` on your laptop, run `sm login` and approve it from your phone, run `sm rc` in a real repo with your own claude login, open the session link in your phone browser (mobile web page; add it to the home screen for push). Follow docs/remote-control-parity.md: start in the terminal, continue on the phone, close the laptop lid, reopen. Note any gaps. Native-app checks come later (M3-016). |
| M3-016 | P7 | Install the build on both phones; run docs/ux/mobile-checklist.md; commit results. |
| M6-002 | P7 | Create a Cloudflare API token scoped to the promotion target account (it may be a different account from the hub's): Account → Workers Scripts Edit, Containers Edit (or "Cloudchamber"), Workers R2 Storage Edit and D1 Edit if the promoted service uses them; Zone → Workers Routes Edit only for a custom domain. Enter it in the app's org settings (stored encrypted, M2-002). Record its scopes (no secret) in docs/ops/deploy-tokens.md. Never paste it into chat. |
| M6-006 | P7 | Add Apple/Google/Windows signing material as CI secrets per docs/ops/release.md; approve store listings. |
