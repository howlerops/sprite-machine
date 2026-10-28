# Ledger status

_Rewritten by the leader on every ledger commit._

done 20/191 (3 dropped) · in-flight MVP-000b,M1B-002 · blocked 8 (human: M0-014, M0-020, M1B-001, M1B-010, M2B-020, M3-016, M6-002, M6-006) · attempts 17/120

MVP-000b merged, waiting for its first CI run; M1B-002 in flight.

## Per phase

| Phase | Items | Done | In flight |
| ----- | ----- | ---- | --------- |
| P0 | 12 | 10 | 1 |
| P1 | 29 | 5 | 1 |
| P2 | 22 | 1 | 0 |
| P3 | 19 | 1 | 0 |
| P4 | 32 | 0 | 0 |
| P5 | 30 | 0 | 0 |
| P6 | 28 | 0 | 0 |
| P7 | 16 | 0 | 0 |

## P0 items

| Item | Owner | Lane | Status | Title |
| ---- | ----- | ---- | ------ | ----- |
| M0-002 | leader | rust | done | Cargo workspace skeleton (all crates incl. client-gpui, runner, edges; web/) + Rust CI incl. macos-latest |
| SPEC-001 | leader | docs | done | Spec: TurnEvent envelope (docs/specs/turn-event.md + JSON Schemas) |
| SPEC-002 | leader | docs | done | Spec: runner ↔ hub tunnel (docs/specs/tunnel.md) |
| MVP-000 | worker | rust | done | Tracer bullet: acp-mock → runner core → tunnel → hub on wrangler dev → headless client |
| M0-003 | worker | client | done | Dev toolchain script (targets, lavapipe/Vulkan + gpui render proof, wrangler, sccache, nextest, worker-build) |
| M0-006 | worker | rust | done | protocol::acp — re-export ACP v1 schema from the official Rust SDK (agent-client-protocol 3.x) |
| M0-017 | worker | rust | done | Spike: Cloudflare Workers (workers-rs + axum, SQLite DO + hibernatable WebSocket, D1, R2) under wrangler dev |
| M1A-002 | worker | rust | done | acp-mock: scriptable ACP v1 agent |
| GPUI-000 | leader | client | done | Pin gpui + gpui-component, vendor their examples, write client-gpui CONVENTIONS.md |
| M1B-002 | worker | rust | assigned | agui-fixture: synthetic TurnEvent stream server (crate name kept) |
| M1B-003 | leader | docs | done | Client-stack gates written before prototypes |
| M1B-004a | worker | client | todo | gpui spike: app shell + virtualized transcript pane |

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
