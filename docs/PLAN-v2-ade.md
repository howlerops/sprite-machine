# sprite-machine v2 — a multi-protocol Agent Development Environment on Sprites

> Status: plan / RFC · rev 5 · 2026-10-27
>
> - Rev 4 incorporated the four-lens review.
> - Rev 5 adds the consistency + strategy reviews and these decisions:
>   - **A2A at the edges**: one internal event envelope everywhere we control;
>   - an **interim mobile web page** for the phone;
>   - **everything stays in v2.0**, delivered in phases;
>   - the **hub runs on Cloudflare**, not Fly.
>
> Goal: a **native app for mobile and desktop** (iOS, Android, macOS, Windows, Linux).
> Connect **any** agent fleet and drive it like Claude Code Remote Control:
>
> - start on your computer, pick it up on your phone;
> - tasks, diffs, approvals, terminals, previews, checkpoints, multi-agent flows.
>
> Where agents run:
>
> - **your machines**, through the `sm` runner;
> - **Fly.io Sprites** (managed cloud);
> - **any A2A agent** (bring-your-own).
>
> The control plane ("hub") runs on **Cloudflare Workers + Durable Objects**. Work is
> executed from `docs/ledger/LEDGER.yaml` with the `ledger` skill; §8 is the start plan.

---

## 1. Protocol stack: our envelope inside, open standards at every edge

Rule: **inside the system we control** (runner/sidecar → hub → our clients) there is
exactly one wire format, the **TurnEvent envelope** (§1.1). **At every edge where a
third party exists**, we speak the open standard:

| Boundary                                                                               | Protocol                                                                                            | Notes                                                                                                                                                                                                                                     |
| -------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Runner/sidecar ↔ coding harness (Claude Code, Codex, opencode, pi, Gemini CLI, Goose…) | **ACP** v1 (stable) over stdio; v2 is draft. Rust crate `agent-client-protocol` (3.x on crates.io). | Sessions, prompt turns, tool calls, diffs, permission requests, client-hosted terminals + fs, plans, modes, slash commands, elicitation, session list/resume/close/usage (stable). Fork is an RFD. Agents come from the **ACP Registry**. |
| Runner/sidecar → hub                                                                   | **TurnEvent envelope over one outbound WebSocket tunnel** (`docs/specs/tunnel.md`)                  | Laptops and Sprites both dial out, so there are no inbound ports anywhere. seq/ack, resume token, offline queue.                                                                                                                          |
| Hub → our clients (desktop app, mobile web page, TUI, native mobile)                   | **TurnEvent envelope** over a WebSocket subscription (`?after=seq`)                                 | Event names are AG-UI-shaped, so the AG-UI adapter is mechanical.                                                                                                                                                                         |
| Hub ↔ BYO agents (inbound)                                                             | **A2A 1.0** client: JSON-RPC, `A2A-Version: 1.0`                                                    | The A2A edge adapter maps A2A → TurnEvent.                                                                                                                                                                                                |
| Our agents & flows → the world (outbound)                                              | **A2A 1.0** server: Agent Cards per agent/flow, JSON-RPC, streaming, push                           | Any A2A client can call our fleet agents and flows.                                                                                                                                                                                       |
| Hub → third-party UIs                                                                  | **AG-UI 1.0** adapter (HTTP+SSE)                                                                    | Lets AG-UI frontends (CopilotKit etc.) drive our agents.                                                                                                                                                                                  |
| Agent-generated widgets (later)                                                        | **A2UI**                                                                                            |                                                                                                                                                                                                                                           |
| Agent ↔ tools / infra                                                                  | **MCP**: per-Loadout servers, the **fleet MCP** (delegation), `sprites.dev/mcp`, `mcp.fly.io`       |                                                                                                                                                                                                                                           |
| Desktop ↔ Zed                                                                          | **ACP** through the local `acp-bridge`                                                              |                                                                                                                                                                                                                                           |

```
 Harness CLIs ─ACP─ runner / sidecar ══TurnEvent tunnel══ HUB (Cloudflare) ══TurnEvent══ desktop app · mobile web · TUI · native mobile
 (your laptop, or a Sprite)                                 │   ├─ A2A edge ── BYO agents (in)  /  A2A clients (out)
                                                            │   └─ AG-UI edge ── third-party UIs
```

### 1.1 The TurnEvent envelope (spec: `docs/specs/turn-event.md`, written by the leader before fan-out)

`{seq, thread_id, task_id, run_id, ts, event}`. `event` is one of these AG-UI-named
variants. Each row lists the ACP source and how the A2A and AG-UI adapters map it:

| TurnEvent                                             | from ACP v1 [v2]                                                   | A2A adapter (out / in)                                                                              | AG-UI adapter                              |
| ----------------------------------------------------- | ------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------- | ------------------------------------------ |
| `RunStarted`                                          | `session/prompt`                                                   | new task in the context (`referenceTaskIds` = previous turn)                                        | `RunStarted`                               |
| `TextMessage{Start,Content,End}`                      | `agent_message_chunk`                                              | status-update message `TextPart`                                                                    | same                                       |
| `Reasoning*`                                          | `agent_thought_chunk`                                              | `DataPart ext/reasoning`                                                                            | same                                       |
| `ToolCall{Start,Args,End,Result}` (upsert by id)      | `tool_call` / `tool_call_update` [v2 upsert]                       | `DataPart ext/tool`                                                                                 | same                                       |
| `Activity{diff}`                                      | diff `{path, oldText, newText}` [v2 `changes` + `git_patch`]       | artifact `<toolCallId>:<path>` (`text/x-diff` + `ext/tool.diff`), chunked with `append`/`lastChunk` | `ActivitySnapshot/Delta`                   |
| `Activity{plan}`                                      | `plan` [v2 `plan_update`]                                          | `ext/plan`                                                                                          | `ActivitySnapshot/Delta`                   |
| `Activity{terminal}`                                  | client terminals (tee'd) [v2 `terminal_output_chunk`]              | artifact `terminalId`, `ext/terminal`                                                               | `ActivitySnapshot/Delta`                   |
| `Interrupt{approval}`                                 | `session/request_permission`                                       | `INPUT_REQUIRED` + `ext/approval`                                                                   | `RunFinished{interrupt}`; resume = new run |
| `Interrupt{form}` / `Interrupt{auth}`                 | `elicitation/create` form / url                                    | `INPUT_REQUIRED` / `AUTH_REQUIRED`                                                                  | interrupt with `responseSchema`            |
| `StateDelta{session}`                                 | `available_commands_update`, `current_mode_update`, config options | `ext/session-config`                                                                                | `StateDelta`                               |
| `StateDelta{usage}`                                   | `usage_update`                                                     | task metadata                                                                                       | `StateDelta`                               |
| `Subagent{Started,Finished,Error}`                    | delegation / flow step tasks                                       | child tasks                                                                                         | same                                       |
| `Step{Started,Finished}`                              | flow steps                                                         | `ext/flow`                                                                                          | same                                       |
| `Checkpoint`                                          | end of turn (Sprite snapshot or shadow-git commit)                 | `ext/checkpoint` artifact                                                                           | `ActivitySnapshot`                         |
| `RunFinished{success}` / `RunError{cancelled\|error}` | `stopReason` [v2 `state_update`] / `session/cancel`                | `COMPLETED` / `CANCELED` / `FAILED`                                                                 | same                                       |
| `Presence`                                            | — (runner/machine online/asleep/offline)                           | —                                                                                                   | `Custom`                                   |
| user `FilePart`                                       | → ACP `image` / `resource`                                         | `FilePart`                                                                                          | —                                          |

**Golden tests**:

- ACP → TurnEvent is the primary mapping.
- TurnEvent → A2A and A2A → TurnEvent are the edge adapters.
- TurnEvent → AG-UI is the UI edge adapter.

All of them live in `crates/protocol`, with the raw source payload preserved in each envelope.

### 1.2 Identity model

| Concept             | ACP                | TurnEvent / hub                                                                                  | A2A edge                           | AG-UI edge          |
| ------------------- | ------------------ | ------------------------------------------------------------------------------------------------ | ---------------------------------- | ------------------- |
| Conversation        | sessionId          | `thread_id` (Thread DO)                                                                          | contextId                          | threadId            |
| One prompt turn     | prompt             | `task_id`                                                                                        | task                               | 1..n runs           |
| Approval round-trip | permission request | `Interrupt` on the task; resume = new `run_id`                                                   | `INPUT_REQUIRED`                   | interrupt → new run |
| Flow run            | —                  | `thread_id = flow_run.id`; child tasks carry their own thread ids only inside `Subagent*` events | one task per step execution        | one thread          |
| Machine             | —                  | Machine DO; agent `kind ∈ {local, managed, byo}`; presence                                       | Agent Card per machine agent (out) | —                   |

- A follow-up prompt is a new task in the same thread.
- A message sent while a task is running is queued as a pending task. It is visible and removable.
- First answer to an interrupt wins; any later answer gets 409.

---

## 2. What changed upstream

| Upstream                                                                                                                                                                                                      | Impact                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Sprites Block Device** (Sprites since 10/6): verified block storage; checkpoints are versioned snapshots; restore ≈ a restart.                                                                              | Checkpoint per turn is cheap and reliable → undo turn. **Restore works only within the same Sprite.** The API has no cross-Sprite restore and no way to create a Sprite from a checkpoint, so fork and template become copy/recipe operations (§4.3; spike M0-010). Restore restarts the environment: the sidecar must be restart-safe. Old Sprites need the storage upgrade, which drops their old checkpoints. |
| **Sprites API 0.1.12**: services (only **one** can own the HTTP port), fs, exec, policies (network / privileges / resources), connectors **gateway**, usage, labels (read-only in the API, settable via CLI). | Sidecar = the sole HTTP service, which also proxies previews. Credentials never enter the Sprite: harnesses call the connector gateway. Per-workspace hardening.                                                                                                                                                                                                                                                 |
| **Lifecycle**: idle ~30 s → warm → cold. Cold = processes restart, in-memory state lost; services don't keep a Sprite awake; the Tasks API does.                                                              | The sidecar holds a Task during a turn, persists its state, and re-attaches ACP sessions after wake (§4.1).                                                                                                                                                                                                                                                                                                      |
| **`mcp.fly.io`**, **`sprites.dev/mcp`**                                                                                                                                                                       | Agent-side tools in Loadouts. Promotion uses the Machines API with an app-scoped deploy token.                                                                                                                                                                                                                                                                                                                   |
| **ACP** registry + Rust SDK (`agent-client-protocol` 3.x), **A2A 1.0**, **AG-UI 1.0**                                                                                                                         | ACP inside the runner; A2A and AG-UI at the edges (§1).                                                                                                                                                                                                                                                                                                                                                          |
| **Cloudflare** Workers + Durable Objects (SQLite, hibernatable WebSockets, alarms), D1, R2, Queues, Containers (GA April 2026)                                                                                | The hub runs here instead of on Fly (§4.2).                                                                                                                                                                                                                                                                                                                                                                      |

---

## 3. Product shape

**Nouns**:

- **Fleet**: agents grouped by labels.
- **Agent**: has an A2A card. Managed agents also have an ACP harness and a Sprite.
- **Workspace**: repo + branch + Sprite + checkpoint lineage.
- **Context**: a conversation.
- **Task**: one turn.
- **Artifact**: diffs, files, terminals, PRs, previews.

**Surfaces**:

0. **Onboarding**: sign in, then either (a) paste a Sprites org token + add a model connector, or (b) add a BYO A2A URL, or (c) **run `sm rc` on your computer and scan the QR code**, which needs no Sprites token, connector or cloud setup at all. Target: first streamed token in **< 2 min** on a phone. BYO-only works with no Sprites token. Every list has an empty state with one primary action.
1. **New task**:
   - repo/issue → workspace (new or existing) → agent → prompt;
   - mode/model when the card advertises them;
   - attachments;
   - Loadout;
   - drafts survive an app kill.
2. **Inbox**:
   - sections Needs you / Running / Done;
   - search + filters (status/agent/workspace);
   - auto-titles, rename, archive;
   - quick Allow/Deny.
3. **Task view**:
   - streamed transcript, reasoning, tool calls, plan, terminals;
   - diffs (unified/split, open the file from a hunk);
   - approval cards: show the pending diff or command and time left, and record who resolved it;
   - composer with slash commands, mode/model switch, queued-message chips, Stop;
   - inline images;
   - per-turn and cumulative cost and context %.
4. **Workspace**:
   - checkpoint timeline: restore, fork, compare;
   - terminal (desktop pane; mobile key bar);
   - files (view on mobile, edit on desktop);
   - preview (hub-signed URL; system browser sheet on mobile);
   - Sprite lifecycle (sleep/wake/delete, storage, memory, cost).
5. **Agents**:
   - add BYO by URL;
   - launch managed (ACP agent + connector + policy + Loadout);
   - card, skills, health.
6. **Settings**:
   - Sprites token (set/rotate/test), connectors, policies and egress view;
   - Loadouts;
   - notifications (event classes × mute × quiet hours);
   - org members and roles.
7. **Multi-agent flows** (§4.9):
   - start a flow from a template or YAML;
   - live graph view (steps, parallel branches, loop iterations, gates) with drill-down into each step's task;
   - pause, resume, cancel (cascading), retry a step;
   - budget and iteration meters;
   - template gallery: ledger (leader/worker), test-fix loop, plan → implement → review, fan-out review + synthesis, best-of-N with a judge;
   - triggers: manual, GitHub events, schedule;
   - agents can also delegate to other fleet agents themselves, through the fleet MCP server.
8. **Remote Control** (§4.10): start work on your computer (terminal, desktop app or Zed) and pick it up on your phone.
   - The session keeps running on your machine with your files, logins, MCP servers and tools.
   - Phone, desktop app and terminal stay in sync and can all send messages and answer approvals.
   - QR code / session link; a machines list with presence.
   - Dispatch a new task to a chosen machine from the phone.
   - "Continue in cloud" moves a local session to a Sprite workspace; "Pull to my computer" brings a cloud one back.
9. **Ship**: GitHub PR; promote to Fly; Open in Zed (desktop).
10. **Desktop power use**: command palette, shortcuts (⌘K, ⌘N, ⌘↵, task navigation).
11. **Cross-cutting**:
    - screen-reader labels and focus order;
    - Dynamic Type and reduced motion;
    - strings in a Fluent catalog (en only at launch);
    - offline queue for messages and approvals.

**Carried over from v1** (must not be lost): auto-titles, image attachments, file
explorer, search/filter, delete/archive, Sprite suspend/resume/delete,
per-message cost, the settings→worker sync (now Loadouts).

**Phone before native mobile**: a **mobile web page** served by the hub (sessions,
transcript, composer, attachments, Allow/Deny, presence; PWA with Web Push) is the
interim phone client from P2 until the native app ships in P7. It is not a full web IDE.

**Non-goals for v2.0**: a desktop-class web client, a marketplace, hosting
third-party agents' compute.

---

## 4. Architecture

### 4.1 Runner core (`sm`), with host modes `local` and `sprite`

One Rust binary runs the agent everywhere: on your laptop (`local`, §4.10) and inside
each Sprite (`sprite`). A `HostMode` trait swaps only what differs between the two.

| Concern       | `local`                                    | `sprite`                                                                                                                                      |
| ------------- | ------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- |
| Transport     | outbound tunnel to the hub                 | **the same outbound tunnel** (no inbound A2A, no per-Sprite bearer)                                                                           |
| Supervision   | user process / desktop app                 | Sprite **service** (restarts on cold wake); it owns the HTTP port **only** for `/preview/<port>/` and `/healthz`                              |
| Staying awake | n/a                                        | Sprites **Task** (5 min expiry, 60 s heartbeat) during a turn and while an approval is pending (approval TTL, default **10 min** for dogfood) |
| Checkpoints   | shadow git commits `refs/sm/checkpoints/*` | Sprites checkpoint after the task settles; keep last K = 20; prune via `sprite-env`                                                           |
| Credentials   | your own logins                            | connector-gateway base URLs only (§4.3)                                                                                                       |
| Identity      | trusted-device credential (keychain)       | per-Sprite machine credential written via the fs API (0600, sidecar user)                                                                     |

**Common core**:

- **ACP client**:
  - session table: thread → harness process, ACP sessionId, state;
  - one prompt in flight per session; further messages queue as pending tasks;
  - advertises fs (confined to the workspace), terminals (PTY, tee'd into `Activity{terminal}`) and elicitation{form,url}.
- **Mapping**: ACP → TurnEvent, with `seq` per thread. A v2 adapter sits behind a feature flag.
- **Durability**: SQLite holds threads, tasks, session table, pending outbound events and the last acked seq. After a restart or cold wake, the next message calls `session/resume` (fallback `session/load`) before the prompt. The hub is the source of truth, and the runner re-syncs after a Sprite restore.
- **Cancellation**: `session/cancel`, and any pending permission is answered `cancelled`. If an approval TTL expires, the task fails with an `expired` reason and the UI offers "retry turn".
- **Attachments**: images become ACP image blocks; other files go into `.sm/attachments/` and are passed as `@path` references.
- **Harness user separation** (sprite mode): the harness runs as a separate Unix user from the runner.

### 4.2 Hub on Cloudflare

The hub is a **Rust Worker** (`workers-rs`, axum through the `http` feature,
compiled to wasm32). There is **no tokio** on Workers, so the `hub` crate and
everything it uses (`protocol`, shared logic) are **runtime-agnostic**. Anything
that needs a full Linux runtime runs in **Cloudflare Containers** (GA since April 2026) behind a Worker binding.

| Cloudflare piece                           | Used for                                                                                                                                                                                                                                                                                                                     |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Thread Durable Object** (SQLite storage) | One per thread / flow run: append-only event log with `seq`, interrupts (first answer wins), runs, pending tasks, subscription fan-out over **hibernatable WebSockets**, catch-up from `?after=seq` (snapshot + replay from a run boundary). Single-threaded per object, which gives ordering and exactly-once seq for free. |
| **Machine Durable Object**                 | One per runner (laptop or Sprite): tunnel endpoint, presence (online / asleep / offline), offline queue of messages and approvals, delivery with ack, dispatch capacity.                                                                                                                                                     |
| **Flow Durable Object**                    | One per flow run: durable state machine; **DO alarms** for timers, timeouts and retries (§4.9).                                                                                                                                                                                                                              |
| **D1**                                     | Org-wide metadata: users, orgs, members/roles, devices, machines, agents (kind, card cache), workspaces, task index with **FTS5** search, notification prefs, audit log, usage rollups.                                                                                                                                      |
| **R2**                                     | Attachments (signed URLs), workspace archives, handoff git bundles/patches, template artifacts + SBOMs, exported transcripts.                                                                                                                                                                                                |
| **Queues**                                 | Push notification fan-out (APNs/FCM/Web Push), BYO A2A push-webhook ingest, usage rollups, audit export.                                                                                                                                                                                                                     |
| **Cron Triggers**                          | Reaper, retention/TTL, scheduled flow triggers, usage pulls from Sprites.                                                                                                                                                                                                                                                    |
| **Secrets Store / Worker secrets**         | Master key for envelope-encrypting per-org Sprites tokens, Fly deploy tokens, GitHub app secret, APNs/FCM keys.                                                                                                                                                                                                              |
| **Workers Static Assets**                  | The **mobile web page** (interim phone client, PWA) and, later, read-only share pages and universal-link files (`apple-app-site-association`, `assetlinks.json`).                                                                                                                                                            |
| **Workers Logs/Traces, Analytics Engine**  | Observability and metrics. Alerts via notifications.                                                                                                                                                                                                                                                                         |
| **Cloudflare Tunnel + Access**             | The private-network path for agents (§4.7).                                                                                                                                                                                                                                                                                  |

**Auth**:

- Humans: device-code pairing (QR) is the primary login for phones and runners. GitHub OAuth (PKCE) is for the desktop app and web page.
- Roles: owner/admin/member/viewer.
- Every mutation is audited.
- Trusted devices can be required per org.

**SSRF**: BYO agent URLs must be https and resolve to a public IP; Worker `fetch`
is used with redirect re-validation. Local machines (`kind: local`) have no URL;
they reach the hub through their tunnel.

**Retention and privacy**:

- Secret-pattern redaction before events are persisted.
- Defaults: events 90 days, terminal streams 30 days, per-org override.
- Deleting a task or thread cascades; an export API.
- Push payloads carry only ids + a generic title.

**Notifications**:

- APNs/FCM for native apps; Web Push for the mobile web page.
- Event classes × mute × quiet hours.
- Actionable Allow/Deny.

**Dev and test loop**: `wrangler dev` (local workerd + Miniflare) runs the whole hub
in this container. Integration tests drive it over HTTP/WebSocket. Pure logic is
unit-tested natively with `cargo test`.

**Deploy**: `worker-build` + `wrangler deploy` from CI. D1 migrations run under
`wrangler d1 migrations`. Bindings and migrations live in `crates/hub/wrangler.toml`.

**Limits to design around**:

- Worker bundle size (keep the wasm small, strip, minimal deps).
- DO storage per object.
- WebSocket message size.
- D1 per-database size: shard per org if needed.

All are checked in a spike (M0-017).

### 4.3 Sprites orchestration

**Tokens**

- Fly tokens are **not** used for the Sprites API: a Fly token returned 401, and nothing documents Fly tokens there.
- The hub uses a **restricted** Sprites org token (name prefix `sm-<org>-`, creation cap) for orchestration.
- A separate **unrestricted / `usage.read`** token is held only by the billing job, for `/v1/usage*` and connector usage.
- CI uses its own restricted token (`sm-ci-`, cap 2).

**Template = recipe**

- A pinned build script plus an artifact tarball (toolchains, ACP adapters, sidecar). Applied to a fresh Sprite, then checkpointed _inside that Sprite_ as its local baseline.
- ACP adapters are installed from a committed lockfile with integrity hashes (`npm ci --ignore-scripts` plus an allowlist of packages permitted to run install scripts).
- Binaries are pinned by sha256; an SBOM is stored with each template version.

**Fork = copy**

- Create a Sprite, apply the recipe, then copy the workspace tree from the source checkpoint (`/.sprite/checkpoints/<id>/` exposes the last five) or after an in-place restore.
- Expect minutes. M0-010 tests whether a faster path exists.

**Per-workspace hardening**

- One label `ws:<id>` (labels via the CLI path or an undocumented create field, checked in M0-006; otherwise the name-prefix `sm-<org>-<ws>-`).
- Network policy: an **explicit allowlist, never the `defaults` preset**.
- Privileges profile `minimal` + `noNewPrivileges`.
- Memory `limit_mb` per harness.

**Credentials via the connector gateway**

- Harness base-URL variables (e.g. `ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL`) point at `api.sprites.dev/v1/gateway/<provider>/<id>`. No credential material exists in the Sprite.
- Connector access policies use the workspace label and narrow `allowed_endpoints`.
- Providers without a native connector use a `custom_api` connector.
- Harnesses that need a subscription OAuth login are marked **unsupported** for managed workspaces. BYO-token-on-disk is possible only behind an explicit warning.

**Lifecycle**

- Admission control against `running_limit` / `warm_limit` and the 10-creates/min limit.
- Reaper: workspaces idle beyond the TTL (default 14 days) are archived to a tarball in object storage, then the Sprite is deleted; restore from archive is supported.
- Budgets per org from `/v1/usage`.

**Promote to production**: the Fly Machines API with an **app-scoped deploy token**
stored per org (create Machine, volume, secrets from the workspace service definition).

### 4.4 Clients: native desktop + mobile, interim mobile web

**Interim mobile web page** (`web/`, P2): a small TypeScript-free page (Rust→wasm via `client-core`, or plain HTML + a few hundred lines of JS against the envelope schema). It covers the session list, transcript, composer with attachments, Allow/Deny, presence and Web Push. It is tested with Playwright in CI.

**Desktop: gpui-kit**, decided on Linux measurements in P0–P3 (no devices needed). Workers get vendored, pinned examples and `crates/client-gpui/CONVENTIONS.md`; views stay thin over `client-core`.

**Native mobile** is decided in P7 on devices, using the gates below.

**Candidates**

- **gpui-kit** (Longbridge, v0.8) on GPUI.
  - Desktop is production-grade: a Tree-sitter/LSP editor, virtual lists, docks; terminal via `alacritty_terminal`.
  - **Mobile is experimental**: the docs say mobile is not a primary target and only the iOS simulator path is documented. The bridge supports **one GPUI view per app lifetime**. IME composition, accessibility and lifecycle hooks are incomplete. There is no webview. `gpui-pre-mobile` pins GPUI 0.3.4 while Kit is on 0.3.9.
- **Flutter** (AOT on all five platforms; Rust `client-core` via `flutter_rust_bridge`; `xterm.dart`).
- **Hybrid**: gpui-kit desktop + Flutter mobile over a shared Rust `client-core` (`flutter_rust_bridge`).

**Decision process (M1b)**

- **Gates are written before the prototypes** (`spikes/bench/gates.yaml`):
  - numeric: frame time, key-to-glyph latency, cold start, RSS, binary size;
  - hard: Japanese/Chinese IME, autocorrect, dictation, image paste, hardware keyboard, VoiceOver/TalkBack on an approval card, Dynamic Type 200%, 10-min background then resume from seq, push tap from a killed app, stacked native screens, a TestFlight upload accepted.
- **Desktop and mobile are decided separately.** The desktop decision needs no devices.
- **Default rule**: any hard-gate failure on gpui-kit mobile → hybrid. Flutter is only rejected if it fails the same gates.
- Flutter is measured on the iPhone too.

**Client-core** (Rust, stack-agnostic):

- TurnEvent subscription client (`?after=seq`) and view models (most UI logic lives here, natively tested);
- SQLite cache;
- JSON-Patch state;
- an **outbound queue** (messages, approvals) with idempotency keys. A 409 shows who resolved an approval;
- backoff with jitter, a connectivity banner;
- iOS foreground-resume catch-up.

### 4.5 Language: Rust end to end

One Cargo workspace (`members = ["crates/*"]`) holds the sidecar, hub, client-core
and client UI. TypeScript survives only inside harness adapters that run as ACP subprocesses.

**Why Rust**:

- gpui-kit is Rust;
- the official ACP Rust SDK;
- a single static (musl) sidecar binary with a few MB of RAM;
- wasmtime for plugins;
- `boringtun` / `libtailscale` for VPNs;
- `alacritty_terminal`;
- one `protocol` crate shared by every layer.

**Costs**:

- the A2A and AG-UI Rust crates are community-maintained. We wrap them behind our own traits, or codegen from the official proto/JSON Schema. Any crate must pass 1.0 wire-format fixtures (PascalCase methods, no `final`/`kind`, `supportedInterfaces`);
- slower iteration;
- compile times. A shared `CARGO_TARGET_DIR` across worktrees helps.

### 4.6 Extensibility: Loadouts and app plugins

A **Loadout** (YAML; org → workspace → task merge) declares:

- harnesses (ACP Registry ids or commands): claude via `claude-agent-acp`, opencode via native `opencode acp`, pi via a pinned community adapter (`pi-acp` / `pi-acpinator`), codex, gemini, goose;
- skills (Agent Skills `SKILL.md` folders);
- MCP servers;
- slash commands and saved prompt templates;
- subagents and hooks;
- `harness_plugins` (Claude marketplace, pi packages) passed through untouched;
- a network profile and an approval policy (auto-allow presets).

**How it is applied**:

- **MCP**: passed per session via ACP `session/new.mcpServers`, with a native-config fallback. Remote MCP auth goes through a `custom_api` connector (bearer). MCP OAuth/DCR servers are unsupported until verified.
- **Renderers per harness** write skills, commands, subagents and hooks into each harness's native layout. Unsupported features are reported, never silently dropped.
- Harnesses are baked into the template where common, otherwise installed lazily and cached by the local checkpoint.
- A **harness conformance matrix** (permissions, diffs, terminals, MCP via session/new, resume, fork-if-advertised, multi-session per process) is generated in CI and shown in the Loadout UI.

**App plugins**:

- Sandboxed WASM components (wasmtime + WIT), deny-by-default capabilities, fuel and memory limits.
- Hooks: task lifecycle, AG-UI/A2UI renderers, agent providers, network profiles, Loadout sources.

**Catalog**:

- Sources: ACP Registry, MCP registry, git.
- Pinned versions; an org allowlist gates harness-native plugins.

### 4.7 Networking and VPNs

Sprites network policy is DNS-based:

- raw IPs are blocked unless resolved from an allowed domain;
- **private IPs are always blocked**, so Fly 6PN / `.internal` cannot be reached from a Sprite.

| Profile             | How                                                                                                                                                                                                                                                                             | Status                                                    |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------- |
| `public`            | explicit allowlist                                                                                                                                                                                                                                                              | default                                                   |
| `tailnet`           | userspace `tailscaled`, DERP over 443, DERP hostnames allowlisted, ephemeral key                                                                                                                                                                                                | **spike M0-008**. A community report says UDP is blocked. |
| `wireguard`         | `boringtun` in the sidecar + SOCKS for the harness; peer must be a hostname                                                                                                                                                                                                     | spike M0-008                                              |
| `cloudflare-tunnel` | The private network runs `cloudflared`, exposing internal services on hostnames protected by **Cloudflare Access** service tokens. The Sprite allowlists those hostnames and presents the service token through a `custom_api` connector, so the token never enters the Sprite. | **preferred**: no VPN inside the Sprite                   |
| `gateway`           | Fallback for non-HTTP protocols: a per-org egress gateway at a **public hostname** (Cloudflare Container or any VM), mTLS / CONNECT proxy. It provides the private hop (tailnet/WireGuard).                                                                                     | fallback                                                  |
| client side         | native apps reach a self-hosted hub over the OS VPN / embedded `libtailscale` on desktop                                                                                                                                                                                        |                                                           |

The UI shows each workspace's effective egress and a denied-domain log.

### 4.8 Security model (Sprite-internal trust)

Any process in a Sprite can call every connector whose policy matches the Sprite.
That includes the harness, plugins and npm postinstall scripts, and the
pre-installed gateway skill teaches agents to list connectors. So:

1. One label per workspace; connector policies scoped to it, with endpoint allowlists (e.g. GitHub `/repos/<org>/*`) and blocked admin paths.
2. An explicit network allowlist, shown as a diff in the UI. The `defaults` preset is never used, because it allows write-capable exfiltration channels (gists, package publish).
3. Privileges `minimal` + `noNewPrivileges`; the harness user is separate from the sidecar user. The sidecar state directory is 0700, and the push token is held in sidecar memory only.
4. The hub validates every inbound event against the push config that authenticated it.
5. Harness-native plugins: org allowlist **and** only on workspaces whose connectors are read-only.
6. An **exfiltration E2E test** (M4) plus `docs/security/threat-model.md` covering:
   - prompt injection;
   - plugins;
   - hub↔sidecar auth;
   - push spoofing;
   - BYO-agent SSRF.

### 4.9 Multi-agent flows and loops

Single delegation is not enough. The hub runs a **durable flow engine**: one
**Flow Durable Object** per run, with SQLite state and **alarms** for timers,
timeouts and retries. Multi-agent work therefore survives Sprites sleeping, clients
disconnecting and Worker restarts. A flow is a declarative graph of steps; each
step that calls an agent is an ordinary task: local, managed, or BYO through the
A2A edge.

**Flow definition** (YAML, versioned; conditions in **CEL** via the `cel` crate):

```yaml
flow: test-fix-review
inputs: { repo: string, goal: string }
budget: { usd: 20, wall_clock: 2h, max_agent_tasks: 40 }
workspace: { strategy: shared } # shared | worktree-per-branch | sprite-per-branch
steps:
  - id: plan
    agent: { select: 'skill:planning', model_hint: opus }
    prompt: 'Plan how to achieve: {{inputs.goal}}'
    output: { schema: plan.json } # structured output, validated
  - id: implement
    loop:
      until: "steps.verify.exit_code == 0 && steps.review.verdict == 'approve'"
      max_iterations: 5
      no_progress: { same_diff_twice: true } # stop if an iteration changes nothing
    steps:
      - id: code
        agent: { select: 'label:worker', model_hint: haiku }
        prompt: "{{steps.plan.output}}\n\nFix: {{steps.review.notes}}"
      - id: verify
        run: 'cargo test --workspace' # executed in the workspace via exec
      - id: review
        agent: { select: 'label:reviewer' }
        judge: { schema: verdict.json } # {verdict: approve|changes, notes}
  - id: gate
    human: { prompt: 'Open a PR?', timeout: 24h, on_timeout: stop }
  - id: ship
    action: github.open_pr
```

**Step types**:

| Type               | What it does                                                                                                                                                                                    |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `agent`            | Sends a task to an agent (local, managed, or BYO through the A2A edge) chosen by id, label or skill. Supports a prompt template, attachments, structured output (JSON Schema) and a model hint. |
| `parallel` / `map` | Runs branches concurrently, or the same steps over a list (e.g. one review per lens). `join: all \| any \| quorum(n)`, with a concurrency limit.                                                |
| `loop`             | `while` / `until` CEL condition, `max_iterations` (required), and no-progress detection (same diff twice, same failing test set).                                                               |
| `branch`           | CEL `if` / `switch`.                                                                                                                                                                            |
| `run`              | A command in the flow's workspace via Sprites exec; exit code, stdout and stderr become step output.                                                                                            |
| `judge`            | An agent step whose output must match a verdict schema; it drives loops and branches.                                                                                                           |
| `human`            | An approval or form gate (`Interrupt`, push notification), with a timeout and `on_timeout` behaviour.                                                                                           |
| `merge`            | Merges branch worktrees or Sprites back into the main workspace; conflicts go to an agent step or a human.                                                                                      |
| `action`           | Built-ins (open PR, promote, notify), plus WASM plugin actions.                                                                                                                                 |
| `ledger`           | The built-in **leader / worker / review** pattern (below).                                                                                                                                      |
| `subflow`          | Calls another flow.                                                                                                                                                                             |

**Built-in `ledger` pattern**: the same process our `ledger` skill uses, as a product feature.

- A leader agent (`model_hint: opus`) turns a goal into a ledger, then assigns ready items to a worker pool (`haiku`) in parallel, each in its own git worktree.
- Every result goes through `run` verify + leader review. A failed item gets a re-attempt that escalates to a stronger model; a passing one is merged.
- It loops until every item is done or waiting on a human.
- The ledger is shown as a live board in the client.

**Workspace strategies** for parallel branches:

- `shared`: sequential steps on one workspace.
- `worktree-per-branch`: one Sprite with a git worktree per branch. This is fast and the default for parallel coding.
- `sprite-per-branch`: full isolation. It's slow because forks are copies (§4.3) and it counts against `running_limit`.

**Agent-initiated delegation**:

- Every managed agent's Loadout includes a **fleet MCP server** served by the hub. Its tools are `list_agents`, `delegate(agent, prompt, wait)`, `await_task`, `cancel_task` and `start_flow`.
- So Claude Code, Codex and the rest can spawn other fleet agents mid-turn. Child tasks link to the parent and show as `Subagent*` events.
- Limits: depth (default 3), fan-out and budget are inherited from the parent.
- Harness-internal subagents (e.g. Claude Code's Task tool) are rendered as nested activity when the ACP tool call identifies them.

**Flows as agents**: a published flow gets its own A2A Agent Card from the hub. Other
agents, other hubs and Zed (through the ACP bridge) can call it like any agent.

**Triggers**: manual, A2A inbound, GitHub webhooks (issue labelled, PR comment
`/agent …`, CI failed), and schedules (a hub cron now; Sprites 2.0 schedules later).

**Guardrails**:

- Budgets enforced before every step (USD from usage + connector spend, wall clock, task count, loop iterations).
- Cancellation cascades to every running task.
- Step retries are idempotent (idempotency key per step × iteration).
- Admission control respects Sprite limits.
- Every flow run is replayable from `flow_steps`.

**Client view**: a flow run is one thread (`thread_id = flow_run.id`); child tasks keep their own threads, which appear only inside `Subagent*` events.

- Steps → `StepStarted/StepFinished`; agent tasks → `Subagent*` (nested `Raw` per child event).
- Flow state (graph, iteration counters, budgets, ledger board) → `StateSnapshot/StateDelta`.
- Human gates → interrupts.

### 4.10 Remote Control: local runner and handoff

Goal: parity with Claude Code Remote Control. Work starts on your computer, keeps
running there, and you pick it up on your phone (or another computer) in one step.
Unlike Claude's, it works for **any ACP agent** and any harness login, and the same
session can later move to the cloud.

**Runner (`sm`)**: the sidecar core with a `local` host mode instead of `sprite`. It
ships as one static binary, and the desktop app embeds it.

| Command                                    | Behaviour                                                                                                                                                                                                                                           |
| ------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `sm login`                                 | Device-code pairing (shown as a QR code). It enrolls the machine as a **trusted device**; the credential lives in the OS keychain.                                                                                                                  |
| `sm rc`                                    | **Server mode.** Waits for sessions started from the phone. `--spawn same-dir \| worktree`, `--capacity N` (default 32), `--permission-mode`, `--name-prefix` (default: hostname), `-c/--continue`. Prints a session URL; spacebar shows a QR code. |
| `sm` / `sm --rc [name]`                    | **Interactive TUI session** that is also live on the phone. You can type in the terminal and on the phone at the same time.                                                                                                                         |
| `/rc` inside the TUI, a desktop-app toggle | Connect an existing local session. **Auto-connect** setting: every local session registers automatically.                                                                                                                                           |
| `sm mobile`                                | QR code for installing the app.                                                                                                                                                                                                                     |

**Connection**:

- The runner makes **outbound HTTPS/WebSocket only** and never opens a port.
- It keeps one WebSocket tunnel to its **Machine Durable Object** on the hub, with heartbeats, seq/ack and a resume token (`docs/specs/tunnel.md`). TurnEvents flow up; prompts, approvals and dispatches flow down.
- The transcript is stored on the hub (same retention and redaction rules as §4.2), so every device stays in sync and a reconnect replays from `seq`.

**Sleep and disconnects**:

- When the laptop sleeps or the network drops, presence goes **asleep / offline**.
- Messages and approval answers sent from the phone are queued on the hub, and delivered in order when the runner reconnects with backoff.
- The phone shows "waiting for my-laptop" and gets a push when the machine is back or needs an approval.
- A running turn whose harness died is resumed through `session/resume`, as in §4.1.

**Local environment**:

- The runner discovers installed harnesses (claude, codex, opencode, pi, gemini, goose) and their ACP adapters, and uses **your existing logins**. That includes Claude subscription logins, which managed Sprites can't use (§7.4).
- It also uses your local MCP servers, tools and project config.
- Files and images sent from the phone land in the workspace and are passed to the agent as `@` file references or image blocks.

**Local checkpoints**: there is no Sprite to snapshot, so each turn is a **shadow git
commit** under `refs/sm/checkpoints/*`. Restore and undo work the same in the UI.

**Dispatch**: the composer's **Where** picker offers your machines (with presence)
and cloud workspaces. Picking a machine starts a session through its `sm rc` server,
in `same-dir` or a new worktree.

**Handoff**:

- **Continue in cloud**: the runner sends a git bundle + an uncommitted-changes patch. The hub creates or forks a Sprite workspace and applies them. The conversation continues there:
  - if the harness's session files are portable (e.g. Claude Code's session JSONL), they are copied and loaded with `session/load`;
  - otherwise the new agent gets the transcript summary + plan as context.
    The phone stays on the same thread.
- **Pull to my computer** (teleport): the reverse. It fetches the branch + patch into a local worktree and resumes there.

**Other front ends on the same session**: the desktop app and Zed (through
`acp-bridge`) attach to a local session, so "start in Zed, continue on the phone" works.

**Private agents**: a runner can also **relay BYO A2A agents** on its LAN or VPN
through its tunnel, which covers private agents without a cloud VPN (§4.7).

**Security**:

- Trusted-device enrollment, plus a per-machine credential that can be revoked from the app. Orgs can _require trusted devices_.
- A **workspace trust** prompt the first time a directory is used.
- The local permission mode applies.
- The phone can't open a raw shell on the computer unless the user turns on "allow remote terminal" for that machine.
- All runner actions are recorded in the audit log.

---

## 5. Milestones and phases

Milestone ids (M0…M6, M2b, M5b, M5c) are stable ledger prefixes. **Phases** decide
the order of work. Everything below is in v2.0; phases only sequence it so that
something usable ships early.

| Phase                                    | Delivers                                                                                                                                                                                                                                          | Main ledger milestones                   | Needs from you                                 |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------- | ---------------------------------------------- |
| **P0 Foundations** (week 1)              | Workspace, dev loop (Vulkan/lavapipe, workerd, sccache), leader-written specs, ACP types, acp-mock, **tracer bullet** (acp-mock → runner → tunnel → hub-on-workerd → headless client, one prompt + one approval), GPUI conventions, desktop gates | M0, M1b (part)                           | nothing                                        |
| **P1 Engine**                            | Runner core (ACP client, mapping, durability, shadow-git checkpoints), hub Worker + Thread/Machine DOs + D1 on local workerd, client-core                                                                                                         | M1a (core), M2, M2b (core), M3-001       | nothing                                        |
| **P2 Remote Control** → `v2.0.0-alpha.1` | `sm rc` / TUI / pairing / QR / trust / discovery (claude + opencode first), **mobile web page**, deployed hub, parity E2E                                                                                                                         | M2b, WEB                                 | **Cloudflare account + API token** (to deploy) |
| **P3 Desktop app** → `alpha.2`           | gpui-kit desktop: shell, composer, task view, approvals, diff, inbox, embedded runner, palette, notifications; unsigned macOS builds from CI                                                                                                      | M3                                       | macOS CI runner enabled; a Mac to dogfood      |
| **P4 Managed cloud** → `beta.1`          | Sprites client + spikes, `sm --host sprite`, keep-alive, checkpoints, template recipe, workspaces, connectors, timeline, terminal/files/preview, handoff, reaper, exfiltration test                                                               | M0 (Sprites part), M1a (sprite part), M4 | Sprites token, model connector                 |
| **P5 Fleet & edges**                     | A2A edge (in + out, TCK), AG-UI edge, Loadouts + renderers (6 harnesses) + conformance, MCP, fleet-MCP delegation, usage, ACP v2                                                                                                                  | M5                                       | harness logins on your laptop for conformance  |
| **P6 Flows, plugins, networking**        | Flow engine + templates + graph view, WASM plugins + catalog, network profiles + Cloudflare Tunnel path                                                                                                                                           | M5c, M5b, M5 (net)                       | tailnet/WireGuard test creds (optional)        |
| **P7 Native mobile & ship** → `v2.0.0`   | Mobile client decision on devices + native app, PR flow, promote to Fly, Zed bridge, signed releases, retire v1                                                                                                                                   | M1b (devices), M3 (mobile), M6           | Apple account, devices, signing material       |

**v1 stays deployed and untouched** until v2 has cloud workspaces, checkpoint
restore, image attachments and per-message cost. Then it is tagged `v1-final` and retired.

---

## 6. Repo plan

```
Cargo workspace (members = ["crates/*"]); gpui + gpui-component pinned by git rev
crates/
  protocol/      turn_event/, acp/, a2a/, agui/, tunnel/, mapping/ (golden tests), schemas/
  runner/        `sm` binary: host modes (local, sprite), tunnel client, TUI, pairing, server mode
  sidecar/       runner core library: acp/, bridge/, store/, keepalive/, checkpoint/, preview/
  hub/           Cloudflare Worker (wasm32, no tokio): routes, DOs (thread, machine, flow), D1, R2, queues
  a2a-edge/      A2A 1.0 client (BYO in) + server (our agents/flows out)
  agui-edge/     AG-UI 1.0 adapter for third-party UIs
  sprites/       generated client + checkpoint NDJSON + exec WS + undocumented fields
  loadout/  netprofile/  plugin-host/  flow/  fleet-mcp/
  acp-mock/  agui-fixture/  acp-bridge/  cli/
  client-core/   view models, sync, cache, outbound queue (most UI logic, natively tested)
  client-gpui/   desktop (and maybe mobile) UI; thin views over client-core
web/             mobile web page (served by the hub via Workers Static Assets)
docs/specs/      turn-event.md, tunnel.md (leader-written before fan-out)
docs/vendor/     pinned gpui-component examples + CONVENTIONS for workers
gateway/  spikes/  scripts/  manifest/ (→ default Loadout)
```

---

## 7. Open questions / risks

1. **Sprites token** (P4) and **Cloudflare account** (P2) are the two account dependencies. P0–P1 need neither.
2. **Workers constraints**: no tokio, wasm bundle size, DO/D1 limits; the `workers-rs` D1 binding is marked alpha. Spike M0-017 confirms these early. Fallback: put the D1 work behind a "directory" DO, or move heavy routes to a Container.
3. **gpui-kit mobile** is experimental. The interim mobile web page de-risks the phone. Native mobile is decided on devices in P7 by the gates (default: hybrid with Flutter if a hard gate fails).
4. **GPUI for Haiku workers**: the API is pre-1.0 and fast-moving (gpui-component 0.8.1 was published 2026-10-26). Mitigations:
   - vendored examples + `CONVENTIONS.md`;
   - the first screen of each kind goes to Opus;
   - thin views over `client-core`;
   - a serialized client lane.
5. **No cross-Sprite restore**: forks are copies (§4.3).
6. **Model credentials in Sprites**: whether there is a native Anthropic connector is unverified (custom_api is the fallback). Subscription logins work only through local runners.
7. **Mapping fidelity**: golden tests per mapping; raw payloads preserved.
8. **Community Rust SDKs** (A2A, AG-UI): they now sit only in the edge adapters, behind traits, checked against 1.0 fixtures.
9. **Harness parity**: the conformance matrix; claude + opencode first.
10. **Remote Control trust**: trusted devices, revocation, workspace trust, local permission mode, remote terminal off by default, audit. Transcripts are stored in the hub for sync.
11. **Runaway multi-agent cost**: required `max_iterations`, no-progress detection, budgets before each step, delegation depth/fan-out limits, cascade cancel.
12. **Sprites limits & cost**: admission control, reaper, retention, budgets; approval TTL 10 min by default.
13. **Ledger-run economics**:
    - full ledger: ≈270–300 worker attempts, ≈$200–450 model spend, 3–5 weeks of continuous running;
    - P0–P2: ≈$60–120 and ~1.5 weeks.
    - Container: 4 vCPU / 15 GB / 30 GB disk.
14. Evaluated and set aside: Tov, Expo/RN, Tauri, Compose Multiplatform. Fly Machines/MPG for the hub were replaced by Cloudflare.

---

## 8. How we start (execution plan)

**Before the first wave** (the leader, in this session or a fresh Opus session):

1. **Ledger settings**:
   - `max_parallel: 2` for Rust items, plus a separate docs/spec lane (3 total);
   - `CARGO_BUILD_JOBS=2`, lld linker, sccache once installed;
   - `max_worker_minutes: 60` for client items;
   - `budget.max_total_attempts` set per phase (P0–P2: 120).
2. **The leader writes**, before any worker starts:
   - `docs/specs/turn-event.md` + JSON Schemas;
   - `docs/specs/tunnel.md` (frames, seq/ack, resume, queue semantics, idempotency keys).

   Workers implement specs; they never invent them.

**Week 1 (P0), all without your input**:

| Order | Who                    | Item                                                                                                                                                                 |
| ----- | ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1     | leader                 | M0-002 workspace skeleton (all crates incl. `client-gpui`, `runner`, `a2a-edge`, `agui-edge`, `web/`), `rust.yml` + `macos-latest` job, lld, profile tweaks          |
| 2     | leader                 | SPEC-001 turn-event spec, SPEC-002 tunnel spec                                                                                                                       |
| 3     | workers (2 Rust lanes) | M0-003 dev script (lavapipe/Vulkan + xvfb screenshot proof, wrangler/workerd, sccache, nextest) ‖ M0-006 ACP types → M1A-002 acp-mock ‖ M0-017 Workers spike         |
| 4     | Opus worker            | MVP-000 **tracer bullet**: acp-mock → runner → tunnel → hub on workerd (in-memory DO) → headless client; one prompt + one approval; `cargo run -p cli -- e2e tracer` |
| 5     | leader                 | GPUI-000: vendor the pinned gpui-component examples + write `crates/client-gpui/CONVENTIONS.md`; M1B-003 desktop gates                                               |
| 6     | workers                | M1B-004a gpui spike (after the lavapipe proof); M3-001 client-core (after SPEC-001)                                                                                  |

**Weeks 2–3 (P1–P2)**:

- Every later item replaces a stub in the tracer bullet.
- `cli e2e tracer` stays green after every merge.
- At the end of P2 you install `sm` on your laptop, run `sm login` (QR) and `sm rc`, open the mobile web page on your phone, and dogfood on a real repo with your own `claude` login. That's `v2.0.0-alpha.1`.

**Asks, in the order they unblock work** (nothing blocks week 1):

1. **Cloudflare account + API token** (Workers Paid, DOs, D1, R2, Queues) as `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID`. Needed by P2 to deploy.
2. **macOS runner** enabled for the repo's GitHub Actions. Needed by P3 dogfood builds.
3. **Sprites token + model connector**. Needed by P4.
4. **Apple account + phones**. Needed by P7.

**How you follow along**:

- `docs/ledger/STATUS.md` + `LOG.md` on the `v2` branch.
- A one-line update from the leader after every batch.
- A `v2.0.0-alpha.N` tag at every phase review, with CI builds attached.
