# Ledger status

_Rewritten by the leader on every ledger commit._

done 110/269 (3 dropped) · in-flight M2B-014d · blocked 12 (human: M0-014, M0-020, M1B-001, M1B-010, CI-001, M2B-020, M3-016, M6-002, M6-006) · attempts 105/200

## Per phase

| Phase | Items | Done | In flight |
| ----- | ----- | ---- | --------- |
| P0 | 12 | 12 | 0 |
| P1 | 37 | 37 | 0 |
| P2 | 68 | 54 | 1 |
| P3 | 23 | 4 | 0 |
| P4 | 52 | 0 | 0 |
| P5 | 30 | 0 | 0 |
| P6 | 28 | 0 | 0 |
| P7 | 16 | 0 | 0 |

## P2 items

| Item | Owner | Lane | Status | Title |
| ---- | ----- | ---- | ------ | ----- |
| MVP-000f | worker | rust | done | Hub wasm: drop the name section (34% of the binary) without strip |
| M0-009 | leader | rust | blocked | Sandbox spike: start/exec/stop on the deployed preview env; cold start + wake timings |
| M0-018 | human | rust | done | Provide a Cloudflare account + API token |
| M1A-004a | worker | rust | done | sidecar terminal/fs hardening: SIGKILL escalation on terminal/kill, openat-style fs I/O (no TOCTOU), cleanup when the waiter thread fails to spawn |
| M1A-006c | worker | rust | done | runner bridge robustness: harness restart, byte-faithful raw updates, session snapshot at start, stale approvals, MCP servers on new/resume |
| M1B-008b | worker | client | done | gpui spike: common workload (5k-line diff, terminal output, one approval card) alongside the streaming transcript |
| M1B-008c | leader | docs | todo | Run spikes/bench/run-desktop.sh for real on Linux and commit results/gpui-kit/desktop_linux |
| M1B-009 | leader | docs | todo | ADR 0001a: desktop client stack (no devices needed) |
| M2-008b | worker | rust | done | hub: POST /v1/threads/{id}/session (client-api §5.5a) + acp-mock session/set_mode and set_config_option |
| M2-008c | worker | rust | done | hub: Thread DO dedupes event_id (24 h) on the internal events route; POST /v1/threads + dispatch call the Thread DO init hook and queue on the Machine DO |
| MVP-000d | worker | rust | done | Tracer onto the real routes: client side on /v1/threads (Thread DO), sidecar bridge stops emitting UserMessage, delete crates/hub/src/tracer |
| M2B-004a | worker | rust | done | Tunnel durability: commit down commands on processed (not on receive), atomic restart_outbound_stream + outbound_since, honour welcome.max_inflight |
| M2B-004b | worker | rust | done | runner: wss:// tunnel (rustls + webpki roots on tokio-tungstenite) |
| M2B-004c | worker | rust | done | runner: presence{asleep} before OS sleep and TunnelSink::thread_open for locally started threads |
| M1A-006d | worker | rust | done | Bridge: the tunnel client is the only down-commit owner (bridge stops calling commit_down_frame on receipt); processed() carries the idempotency key |
| SPEC-004 | leader | docs | done | Spec: dispatch/session carry per-thread MCP servers; an interrupt outcome / RunError code for "agent gone" (harness_exited) instead of expired |
| M1A-006e | worker | rust | done | Production runner uses Bridge::spawn_with (agent respawn); stored interrupt of an unknown thread answers unknown_interrupt (409); acp-mock records requests and can advertise modes/config |
| M2B-004d | worker | rust | done | runner: OS sleep hook calls SleepHint::asleep (macOS willSleep, Linux logind PrepareForSleep); TunnelSink tracks opened threads (thread_open once per thread, events for an unopened local thread refused) |
| M2-008d | worker | rust | done | hub authz: POST /v1/threads only for a machine of the caller's org (not revoked); POST /v1/threads in the auth policy table |
| CI-002 | worker | docs | done | scripts/hub-test.sh and the tracer build the hub Worker from a per-worktree wasm target dir (shared target lets another worktree's hub.wasm stand in) |
| M1A-006f | worker | rust | done | Tracer bridge built with Bridge::spawn_with (agent respawn); mapper harness-exit ending per SPEC-004 for every open interrupt; remove acp::reattach::fail_stale_interrupts |
| M2B-004e | worker | rust | done | runner: start the sleep watcher at startup with a logind delay inhibitor; macOS sleep watcher (IOKit) implemented |
| M2-008e | worker | rust | done | hub authz (security): every thread route checks the thread belongs to the caller's org (D1 thread_hosts(thread_id, org_id, machine_id)); unknown or foreign thread = 404 |
| M1A-006g | worker | rust | done | Local vs hub prompts by an explicit IncomingCommand flag (not the "local:" idempotency-key prefix); hub refuses "local:" keys; tidy stale tracer wording |
| M2B-005a | worker | rust | done | hub (security): enforce require_trusted_devices at the tunnel; rate-limit device code start and approve attempts; purge expired pairings; runner shows the approving org/user and asks to confirm before storing the credential |
| M2B-005b | worker | rust | done | runner: sm logout / sm status; run mode builds TunnelClient from KeychainCredentials + stored hub URL; Linux persistent keychain decision |
| M2B-006a | worker | rust | blocked | Confirm each harness ACP invocation (claude-agent-acp, codex-acp, opencode acp, pi adapter, gemini --experimental-acp, goose acp) against the real CLI; correct harness::HARNESSES |
| M2B-006c | worker | rust | done | Local MCP config: resolve bare stdio commands to absolute paths, sse entries when the agent supports them, ${VAR} expansion |
| M2B-004f | worker | rust | todo | runner: macOS sleep watcher builds and works (IOKit FFI checked against the SDK, a real sleep/wake on a Mac); logind inhibitor tested against a mock login1 on a private bus |
| M2B-007a | worker | rust | done | sm rc follow-ups: permission mode applied before the first prompt; agent mcp_capabilities exposed by the bridge so http/sse MCP servers pass; stale worktree sweep at start; session end (close command / idle timeout) |
| SPEC-005 | leader | docs | done | Spec: browser sign-in for the mobile web page (runner QR link; browser device code) |
| M2-003c | worker | rust | done | hub: browser sign-in routes per client-api §5.7 (signin links minted by a paired machine; link redeem; user device-code flow); sm rc prints the QR |
| M2B-007b | worker | rust | done | sm rc: a prompt for a thread whose idle session was closed reopens it (recreate the worktree from its branch, resume the ACP session); acp-mock scripts can advertise mcp_capabilities; local close command |
| M2B-005c | worker | rust | done | hub (security): turning require_trusted_devices on closes live tunnels of machines that are no longer trusted; per-IP limit on POST /v1/devices/token |
| M2-011a | worker | rust | done | Push follow-ups: hub-test.sh passes a random VAPID_PRIVATE_KEY (drop the dev key derivation from ATTACHMENT_SIGNING_KEY); retry only the failed subscriptions of a job (no duplicate pushes on partial failure) |
| M2-010a | worker | rust | done | Task index follow-ups: a deleted thread is 404 on every route; backfill/reconcile index rows (pre-0008 threads, failed upserts); a `done` status producer; hub-test passes app-link vars for a live 200 check |
| M2B-002b | worker | rust | done | hub: GET /v1/machines returns the client-api §5.1 shape (machine_id, presence{state,since}, capabilities) for members; live test that /v1/dev/fixtures/* are 404 without DEV_FIXTURES |
| SPEC-006 | leader | docs | done | Spec: browser-usable auth for the thread subscription (single-use subscribe ticket) |
| M2-008h | worker | rust | done | hub: POST /v1/threads/{id}/subscribe-ticket + ?ticket= on the subscribe upgrade (SPEC-006) |
| M2-008a | worker | rust | done | hub Thread DO hardening: secret redaction before persist, catch-up streaming cap, state-history compaction, DEV_FIXTURES via hub-test.sh |
| M2-009 | worker | rust | done | hub: attachments in R2 + signed URLs |
| M2-010 | worker | rust | done | hub: titles, rename, archive/delete, D1 FTS5 search + deep links |
| M2-011 | worker | rust | done | hub: notifications via Queues (Web Push, APNs, FCM) with prefs + actionable approvals |
| CI-001 | human | rust | blocked | GitHub Actions jobs stopped starting on v2 (all jobs fail in ~3 s with no logs since run 51, 2026-10-28T04:47Z) |
| M2-014 | leader | rust | blocked | Provision Cloudflare resources (D1 db, R2 bucket, queues, DO migrations) with wrangler |
| M2-014a | human | rust | todo | Set hub Worker secrets after the first deploy (MASTER_KEY, BOOTSTRAP_OWNER_TOKEN, VAPID_PRIVATE_KEY) + VAPID public key var |
| M2-015 | worker | rust | todo | hub: deploy workflow (wrangler), DR doc (D1 Time Travel, R2), alerts doc |
| M2B-005 | worker | rust | done | runner: `sm login` device-code pairing + trusted device + keychain |
| M2B-006 | worker | rust | done | runner: local harness discovery + local logins/MCP passthrough |
| M2B-007 | worker | rust | done | runner: `sm rc` server mode (spawn, capacity, permission mode, names, --continue) |
| M2B-008 | worker | rust | done | runner: workspace trust prompt + remote-terminal opt-in |
| M2B-009 | worker | rust | done | runner: session URL, QR code, `sm mobile`, auto-connect setting |
| M2B-010 | worker | rust | done | runner: interactive TUI shared with remote devices (`sm --rc`) |
| M2B-011a | worker | rust | done | runner checkpoint: encode thread ids that git refnames reject (protocol ids allow ":") |
| M2B-012 | worker | rust | done | Attachments from phone → local files / image blocks |
| M2B-014 | worker | rust | done | Presence + reconnect notifications on the phone |
| M2B-019 | worker | rust | todo | E2E: start on computer, continue from the phone (mobile web + headless client), survive a dropped tunnel |
| M2B-020 | human | rust | blocked | Real-device Remote Control check |
| WEB-001 | worker | rust | done | web: mobile web page — pairing (QR / device code) + session list with presence |
| WEB-002 | worker | rust | done | web: transcript rendering for every TurnEvent variant |
| WEB-003 | worker | rust | done | web: composer with attachments + Allow/Deny + Stop |
| WEB-004 | worker | rust | done | web: PWA + Web Push |
| M2B-005f | worker | rust | done | hub: Machine DO disconnect action (close the tunnel with auth_revoked without marking the DO revoked); the trust-policy flip uses it, so turning the setting off lets the machine reconnect |
| M2B-009a | worker | rust | done | runner: sm rc prints the session URL for each session it starts and the spacebar toggles its QR (wire runner::qr into server mode) |
| M2-008k | worker | rust | done | hub: DELETE /v1/threads/{id}/pending/{task_id} is member-level (client-api §5.3/§5.5) instead of admin; the web composer uses it instead of the cancel workaround |
| M2B-014d | worker | rust | assigned | notify: machine_back goes to every subscription of each user (like the other kinds) and its payload carries machine_id; the title names the machine |
| M2B-010a | worker | rust | todo | hub+tunnel: an interrupt answer typed in the sm TUI is arbitrated by the Thread DO (first answer wins against the phone), so the hub log and the agent agree |
| M2B-010b | worker | rust | todo | cli e2e tui: sm TUI session with acp-mock against wrangler dev (/rc registers, a phone prompt shows in the TUI, tunnel drop and restore) |

## Waiting on the human

| Item | Phase | Needed |
| ---- | ----- | ------ |
| M0-014 | P6 | Add TS_AUTHKEY (ephemeral, pre-authorized Tailscale key) and WG_TEST_PEER (a WireGuard peer config with a public hostname endpoint) as environment secrets; restart the session. |
| M0-020 | P4 | Cloudflare dashboard → Secrets Store (account level) → create a store if there is none → Create secret: a name such as ANTHROPIC_API_KEY, the value = your model API key (e.g. Anthropic), scope Workers. Tell the leader only the secret name and the store id. Never paste the key into chat. The Workspace DO outbound handler injects it for api.anthropic.com (plan §4.3); it never enters a sandbox. |
| M1B-001 | P7 | Provide an Apple Developer account, a physical iPhone, a mid-range Android phone, and a Mac or approval for GitHub macOS runners (iOS cannot build in the Linux container). |
| M1B-010 | P7 | Build both spikes on a Mac per their READMEs; run spikes/bench on the iPhone and Android; commit results to spikes/bench/results/ with screen recordings for hard gates. |
| CI-001 | P2 | Open https://github.com/howlerops/sprite-machine/actions/runs/40643390008 and read the annotation on any job. The usual cause for jobs that end in seconds with no log is account billing: Actions minutes used up or a spending limit of $0 (macOS minutes count 10x). Fix it under the account or org Settings -> Billing and plans -> Spending limits / payment method. If the annotation says something else, tell the leader. Nothing in the workflow changed between the last green run (50) and the first failed one (51). |
| M2B-020 | P2 | After the hub is deployed (M2-015): install `sm` on your laptop, run `sm login` and approve it from your phone, run `sm rc` in a real repo with your own claude login, open the session link in your phone browser (mobile web page; add it to the home screen for push). Follow docs/remote-control-parity.md: start in the terminal, continue on the phone, close the laptop lid, reopen. Note any gaps. Native-app checks come later (M3-016). |
| M3-016 | P7 | Install the build on both phones; run docs/ux/mobile-checklist.md; commit results. |
| M6-002 | P7 | Create a Cloudflare API token scoped to the promotion target account (it may be a different account from the hub's): Account → Workers Scripts Edit, Containers Edit (or "Cloudchamber"), Workers R2 Storage Edit and D1 Edit if the promoted service uses them; Zone → Workers Routes Edit only for a custom domain. Enter it in the app's org settings (stored encrypted, M2-002). Record its scopes (no secret) in docs/ops/deploy-tokens.md. Never paste it into chat. |
| M6-006 | P7 | Add Apple/Google/Windows signing material as CI secrets per docs/ops/release.md; approve store listings. |
