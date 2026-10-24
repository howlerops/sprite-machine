# sprite-machine v2 — a multi-protocol Agent Development Environment on Sprites

> Status: plan / RFC · rev 4 · 2026-10-23 (incorporates the four-lens review: protocols,
> infra/security, product/client, ledger executability)
> Goal: a new **native app for mobile and desktop** (iOS, Android, macOS, Windows,
> Linux; no web at launch) where you connect **any** agent fleet and drive it like
> Claude Code Remote: tasks, diffs, approvals, terminals, previews, checkpoints.
> Managed agents run on **Sprites**. Bring-your-own agents connect over **A2A**.
> Work is executed from `docs/ledger/LEDGER.yaml` using the `ledger` skill.

---

## 1. Protocol stack: one protocol per boundary

Rule: **each boundary speaks the open standard built for it**. We translate in
exactly two places: the sidecar (ACP ⇄ A2A) and the hub (A2A ⇄ AG-UI).

| Boundary                                                                        | Protocol                                                                                                                                                     | Notes                                                                                                                                                                                                                                                                                                    |
| ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Sidecar ↔ coding harness (Claude Code, Codex, Gemini CLI, Goose, opencode, pi…) | **ACP** (Zed's Agent Client Protocol), JSON-RPC over stdio. Target **v1** (stable); **v2 is draft**. Official Rust/TS SDKs 1.0.                              | Covers sessions, prompt turns, tool calls, diffs, permission requests, client-hosted terminals + fs (v1), plans, modes, slash commands, elicitation, session list/resume/close/usage (stable). **Fork is still an RFD.** Agents are installable from the **ACP Registry**. Replaces v1's JSONL adapters. |
| Hub ↔ agents (managed and BYO), agent ↔ agent                                   | **A2A 1.0**: JSON-RPC binding first (HTTP+JSON later). PascalCase methods; `A2A-Version: 1.0` on every request (an empty header means 0.3).                  | Agent Cards, tasks, streaming, push webhooks for agents that sleep, `INPUT_REQUIRED` / `AUTH_REQUIRED`, extensions via `A2A-Extensions`.                                                                                                                                                                 |
| Hub ↔ client UI                                                                 | **AG-UI 1.0**, standard HTTP+SSE binding, plus **our subscription endpoint** for fan-out/replay (§4.2). The Rust and Dart SDKs are **community-maintained**. | Text, tool calls, reasoning, state snapshot/delta (RFC 6902), activity, subagents, interrupts (`RunFinished{outcome: interrupt}` → resume with a new run).                                                                                                                                               |
| Agent-generated widgets (later)                                                 | **A2UI** over AG-UI                                                                                                                                          | Forms, pickers and cards that the client validates.                                                                                                                                                                                                                                                      |
| Agent ↔ tools / infra                                                           | **MCP**: per-Loadout servers, `sprites.dev/mcp`, `mcp.fly.io` as _agent-side_ tools                                                                          | Production promotion uses the Fly Machines API (§4.3), not an interactive MCP.                                                                                                                                                                                                                           |
| Desktop app ↔ Zed (bonus)                                                       | **ACP** through a local stdio↔hub bridge                                                                                                                     | ACP remote transport is a draft RFD, so we don't build on it.                                                                                                                                                                                                                                            |

```
 Harness CLIs ──ACP v1 (stdio)── sidecar ──A2A 1.0── HUB ──AG-UI (SSE + subscription)── Native clients
 (claude-agent-acp, codex-acp,   (Sprite service,     │  ▲                              (gpui-kit / Flutter)
  opencode acp, pi adapters…)     sole HTTP port)     │  └─A2A── BYO agents
                                                      └─ local ACP bridge ── Zed (bonus)
```

### 1.1 Event mapping

The sidecar normalizes ACP into an internal **`TurnEvent`** enum, so an ACP v2
adapter can be added without touching the A2A side. The v2 names are given in brackets.

| ACP v1 (→ sidecar)                                                                     | A2A 1.0 (sidecar → hub)                                                                                                                              | AG-UI (hub → client)                                                                             |
| -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `session/new` (or `session/resume`)                                                    | new `contextId` (= ACP sessionId)                                                                                                                    | thread (`threadId` = contextId)                                                                  |
| `session/prompt`                                                                       | `SendStreamingMessage` → **new task** in the context; `referenceTaskIds` = previous turn                                                             | `RunStarted` (hub-generated `runId`)                                                             |
| `agent_message_chunk`                                                                  | `TaskStatusUpdateEvent` with message `TextPart` (`messageId` stable per chunk group)                                                                 | `TextMessageStart/Content/End`                                                                   |
| `agent_thought_chunk`                                                                  | `DataPart` `ext/reasoning`                                                                                                                           | `Reasoning*`                                                                                     |
| `tool_call` / `tool_call_update` [v2: `tool_call_update` upsert]                       | `DataPart` `ext/tool`, upsert by `toolCallId`                                                                                                        | `ToolCallStart/Args/End/Result`                                                                  |
| tool content `diff {path, oldText, newText}` [v2: `changes` + `git_patch`]             | artifact `artifactId = <toolCallId>:<path>`, parts: `TextPart text/x-diff` + `DataPart ext/tool.diff`; large diffs chunked with `append`/`lastChunk` | `ActivitySnapshot(activityType: "diff")` after `lastChunk`; `StateDelta /workspace/files/<path>` |
| `plan` [v2: `plan_update{planId}`]                                                     | `DataPart` `ext/plan` (carries planId)                                                                                                               | `ActivitySnapshot/Delta(activityType: "plan")`                                                   |
| client-hosted `terminal/*` (output tee'd by the sidecar) [v2: `terminal_output_chunk`] | artifact `artifactId = terminalId`, `ext/terminal {bytes_b64, exit?}` chunks with `append`/`lastChunk`                                               | `ActivitySnapshot/Delta(activityType: "terminal")`                                               |
| `session/request_permission`                                                           | `TASK_STATE_INPUT_REQUIRED` + `ext/approval {options}`                                                                                               | `RunFinished{outcome: interrupt}`; reply = new run with `resume`                                 |
| `elicitation/create mode=form`                                                         | `INPUT_REQUIRED` + `ext/elicitation {requestedSchema}`                                                                                               | interrupt with `responseSchema` = the ACP schema                                                 |
| `elicitation/create mode=url`                                                          | `TASK_STATE_AUTH_REQUIRED` + URL                                                                                                                     | interrupt (auth)                                                                                 |
| `available_commands_update`, `current_mode_update`, config options                     | `ext/session-config`                                                                                                                                 | `StateDelta /session/{commands,mode,model}`                                                      |
| client → `session/set_mode` / config option                                            | `SendMessage` with `ext/session-config` DataPart                                                                                                     | client action                                                                                    |
| user image/file                                                                        | `FilePart` (hub-hosted signed URI)                                                                                                                   | — (client uploads to the hub)                                                                    |
| → ACP `image` / `resource` content block                                               |                                                                                                                                                      |                                                                                                  |
| `usage_update`                                                                         | task metadata `ext/usage`                                                                                                                            | `StateDelta /usage` (per turn + cumulative)                                                      |
| sub-agent                                                                              | child A2A task via hub delegation                                                                                                                    | `SubagentStarted/Finished/Error`                                                                 |
| `stopReason: end_turn` (+ our checkpoint) [v2: `state_update` idle]                    | task `COMPLETED` + `ext/checkpoint` artifact                                                                                                         | `RunFinished{success}` + `ActivitySnapshot(checkpoint)`                                          |
| `session/cancel` (pending permission answered `cancelled`)                             | `CancelTask` → `WORKING` → `CANCELED`                                                                                                                | `RunError{code: "cancelled"}`; open interrupts abandoned                                         |

Every sidecar event carries **`ext/seq {task_seq}`** in its metadata, so the hub can
dedupe stream and push deliveries exactly.

**Our extensions** (`https://sprite-machine.dev/ext/<name>/v1`, all `required: false`):
`seq`, `workspace`, `tool`, `plan`, `approval`, `elicitation`, `terminal`, `usage`,
`checkpoint`, `reasoning`, `session-config`, `flow` (flow run id, step id, iteration, parent task).

- Extension parts are only emitted when the request's `A2A-Extensions` header names them, and each Message/Artifact lists the URIs it uses.
- BYO agents without extensions render as plain chat + artifacts. Their dedup is best-effort, on a structural key.

### 1.2 Identity model

| Concept             | ACP                | A2A                                                                                                           | AG-UI                                                                   | Hub table                                             |
| ------------------- | ------------------ | ------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------- | ----------------------------------------------------- |
| Conversation        | sessionId          | contextId                                                                                                     | threadId                                                                | `contexts`                                            |
| One prompt turn     | prompt request     | task (terminal at end of turn)                                                                                | 1..n runs                                                               | `tasks`                                               |
| Approval round-trip | permission request | `INPUT_REQUIRED` on the same task                                                                             | run ends in interrupt; resume = new run                                 | `interrupts`, `runs(task_id, resume_index)`           |
| Flow run            | —                  | one task per step execution, in the step's own context; `referenceTaskIds` + `ext/flow` link it to the parent | one thread; `StepStarted/Finished` per step, `Subagent*` per agent task | `flow_runs`, `flow_steps(run_id, step_id, iteration)` |

- A follow-up prompt creates a **new task** in the same context.
- A message sent while a task is `WORKING` is accepted as a new task in `SUBMITTED` state and queued until the session is idle. It is visible to the client and can be removed.
- `RunAgentInput.messages` carries only the latest user message, because history lives in the harness. This is a documented deviation from AG-UI's "complete history".
- First answer to an interrupt wins; any later answer gets 409.

---

## 2. What changed upstream

| Upstream                                                                                                                                                                                                      | Impact                                                                                                                                                                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Sprites Block Device** (Sprites since 10/6): verified block storage; checkpoints are versioned snapshots; restore ≈ a restart.                                                                              | Checkpoint per turn is cheap and reliable → undo turn. **Restore works only within the same Sprite.** The API has no cross-Sprite restore and no way to create a Sprite from a checkpoint, so fork and template become copy/recipe operations (§4.3; spike M0-010). Restore restarts the environment: the sidecar must be restart-safe. Old Sprites need the storage upgrade, which drops their old checkpoints. |
| **Sprites API 0.1.12**: services (only **one** can own the HTTP port), fs, exec, policies (network / privileges / resources), connectors **gateway**, usage, labels (read-only in the API, settable via CLI). | Sidecar = the sole HTTP service, which also proxies previews. Credentials never enter the Sprite: harnesses call the connector gateway. Per-workspace hardening.                                                                                                                                                                                                                                                 |
| **Lifecycle**: idle ~30 s → warm → cold. Cold = processes restart, in-memory state lost; services don't keep a Sprite awake; the Tasks API does.                                                              | The sidecar holds a Task during a turn, persists its state, and re-attaches ACP sessions after wake (§4.1).                                                                                                                                                                                                                                                                                                      |
| **`mcp.fly.io`**, **`sprites.dev/mcp`**                                                                                                                                                                       | Agent-side tools in Loadouts. Promotion uses the Machines API with an app-scoped deploy token.                                                                                                                                                                                                                                                                                                                   |
| **ACP** registry + SDK 1.0, **A2A 1.0**, **AG-UI 1.0**                                                                                                                                                        | Standard wire formats at each boundary.                                                                                                                                                                                                                                                                                                                                                                          |

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

0. **Onboarding**: sign in, then either (a) paste a Sprites org token + add a model connector, or (b) add a BYO A2A URL. Target: first streamed token in **< 2 min** on a phone. BYO-only works with no Sprites token. Every list has an empty state with one primary action.
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
8. **Ship**: GitHub PR; promote to Fly; Open in Zed (desktop).
9. **Desktop power use**: command palette, shortcuts (⌘K, ⌘N, ⌘↵, task navigation).
10. **Cross-cutting**:
    - screen-reader labels and focus order;
    - Dynamic Type and reduced motion;
    - strings in a Fluent catalog (en only at launch);
    - offline queue for messages and approvals.

**Carried over from v1** (must not be lost): auto-titles, image attachments, file
explorer, search/filter, delete/archive, Sprite suspend/resume/delete,
per-message cost, the settings→worker sync (now Loadouts).

**Non-goals for v2.0**: a web client (a hub-served read-only task page may come
later), a marketplace, hosting third-party agents' compute.

---

## 4. Architecture

### 4.1 Sidecar (in each Sprite)

**Process model**

- Runs as the Sprite's **only HTTP service** (`http_port 8080`, `needs: []`, its own `dir`).
- The Sprite URL stays `auth: sprite`. Routes are split by path:
  - `/a2a/*`: requires a **per-Sprite hub bearer**. The hub writes it through the fs API to a 0600 file owned by the sidecar user, never into service env.
  - `/.well-known/agent-card.json` and `/healthz`: readable.
  - `/preview/<port>/*`: reverse proxy to the user's dev server (WebSocket upgrade supported). Only reached through hub-signed preview URLs.
- The harness runs as a **separate Unix user** from the sidecar.

**ACP client**

- **Session table**: contextId → harness process, ACP sessionId, state ∈ {idle, prompting, awaiting_input}. One prompt in flight per session; further messages queue as `SUBMITTED` tasks.
- Advertises `fs`, `terminal` and `elicitation{form,url}`. fs is confined to the workspace. Terminals are PTYs whose output is tee'd into `ext/terminal` artifacts.
- `TurnEvent` normalization layer; v1 adapter now, v2 adapter behind a feature flag (M5).

**A2A server**

- JSON-RPC binding; `A2A-Version` enforced (missing or non-1.0 → `VersionNotSupportedError`).
- Push configs: https-only, no private/link-local targets, `application/a2a+json`, 10–30 s timeout, backoff, disabled after N failures.

**Durability**

- SQLite under the workspace data directory holds tasks, contexts, push configs, the session table and the outbound event seq.
- On restart or cold wake, the next message for a known context calls `session/resume` (or `session/load`) before prompting.
- **The hub is the source of truth.** After a checkpoint restore rewinds the sidecar's store, the sidecar re-syncs from the hub.

**Staying awake**

- Holds a Sprites **Task** (5 min expiry, 60 s heartbeat) while a turn runs. Released at end of turn, on cancel, or by a crash trap.
- While a task is `INPUT_REQUIRED`, it holds a Task up to an **approval TTL** (default 30 min, configurable). On expiry it answers the ACP permission `cancelled`, marks the task `FAILED` with `ext/approval {expired}`, and the hub offers "retry turn".

**Checkpoints**

- Taken after the task reaches a terminal or `INPUT_REQUIRED` state.
- The sidecar tolerates being stopped or restarted by its own checkpoint (verified in M0-006).
- Keeps the last K (default 20) turn checkpoints plus named ones. Pruning goes through the in-Sprite `sprite-env` tooling, because the API has no delete endpoint.

**Agent card**: managed sidecars are **not** signed; trust comes from the hub
provisioning the Sprite plus the bearer. For BYO cards, the hub verifies a JWS when present.

### 4.2 Hub

**A2A ingest**

- Sends `A2A-Version: 1.0` + `A2A-Extensions` on every request.
- Live `SubscribeToTask` while anyone watches; push webhooks otherwise.
- Every event goes to an append-only `task_events` log with a hub seq, **deduped exactly on (taskId, ext/seq)**.
- The `Task` that opens a stream or subscription is a **reconciliation snapshot**: diffed against the projection, never appended.

**AG-UI out**

- Standard `POST /threads/{threadId}/runs` (RunAgentInput → SSE).
- **Our subscription endpoint** `GET /threads/{threadId}/events?after=<seq>`, which all clients read for fan-out:
  - catch-up = `StateSnapshot` + `MessagesSnapshot` + runs from the last run boundary;
  - then live events.
- Replay never starts inside an open message, tool call or subagent.
- The projection is deterministic: message and tool ids are derived from ACP ids in the raw payload.
- Resume input is idempotent by a client-supplied id.

**Data**

- Postgres on Fly MPG, version chosen up front because there are no in-place major upgrades.
- Tables: users, orgs, members(role), contexts, tasks (title, title_source, archived_at, labels, search index), runs, interrupts, task_events, artifacts, attachments, checkpoints, workspaces, agents, push_targets, notification_prefs, audit_log, usage_daily.
- Hub tests run against a local Postgres (`scripts/pg-test.sh`), CI against a service container.

**Auth and authorization**

- GitHub OAuth with PKCE for native clients.
- Roles: owner/admin/member/viewer. Viewers cannot send or approve; destructive operations need admin.
- Every mutation writes an audit row.

**Secrets at rest**

- Envelope encryption under a hub master key (Fly secret) for:
  - the Sprites tokens (§4.3);
  - the GitHub app secret;
  - APNs/FCM keys;
  - Fly deploy tokens.
- Rotation runbook in `docs/ops/`.

**SSRF**

- BYO agent URLs must be https and resolve to a public IP. Re-checked at connect time and on redirects.
- Card size and time limits.

**Push webhooks**

- One random token per push config, bound to (agent, task), compared in constant time.
- The payload's task must belong to that agent.
- Token revoked at task end; endpoint rate-limited per agent.

**Notifications**

- APNs/FCM.
- Event classes: needs_input, completed, failed, pr_opened, delegated.
- Per agent/workspace mute and quiet hours.
- Actionable Allow/Deny.
- Payloads carry only the task id + a generic title, never prompt text.

**Retention and privacy**

- Secret-pattern redaction at ingest, before persistence.
- Defaults: 90 days for events, 30 days for terminal streams, per-org override.
- Deleting a task cascades to its events, artifacts and push configs.
- An export API.

**Ops**

- OpenTelemetry traces and Prometheus metrics.
- Alerts: hub 5xx, Sprites API 429/5xx, spend per org per day, MPG storage > 70%.
- Rate limits.
- Quarterly PITR restore drill.
- Expand/contract migrations under an advisory lock.
- Sidecar service logs shipped to the hub before a Sprite is archived.

**Attachments**: hub blob store (Fly Tigris), signed URIs, ≤ 20 MB.

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

### 4.4 Native clients (mobile + desktop, no web)

**Candidates**

- **gpui-kit** (Longbridge, v0.8) on GPUI.
  - Desktop is production-grade: a Tree-sitter/LSP editor, virtual lists, docks; terminal via `alacritty_terminal`.
  - **Mobile is experimental**: the docs say mobile is not a primary target and only the iOS simulator path is documented. The bridge supports **one GPUI view per app lifetime**. IME composition, accessibility and lifecycle hooks are incomplete. There is no webview. `gpui-pre-mobile` pins GPUI 0.3.4 while Kit is on 0.3.9.
- **Flutter** (AOT on all five platforms; community AG-UI Dart SDK; `xterm.dart`).
- **Hybrid**: gpui-kit desktop + Flutter mobile over a shared Rust `client-core` (`flutter_rust_bridge`).

**Decision process (M1b)**

- **Gates are written before the prototypes** (`spikes/bench/gates.yaml`):
  - numeric: frame time, key-to-glyph latency, cold start, RSS, binary size;
  - hard: Japanese/Chinese IME, autocorrect, dictation, image paste, hardware keyboard, VoiceOver/TalkBack on an approval card, Dynamic Type 200%, 10-min background then resume from seq, push tap from a killed app, stacked native screens, a TestFlight upload accepted.
- **Desktop and mobile are decided separately.** The desktop decision needs no devices.
- **Default rule**: any hard-gate failure on gpui-kit mobile → hybrid. Flutter is only rejected if it fails the same gates.
- Flutter is measured on the iPhone too.

**Client-core** (Rust, stack-agnostic):

- AG-UI subscription client;
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

| Profile     | How                                                                                                                                                    | Status                                                    |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------- |
| `public`    | explicit allowlist                                                                                                                                     | default                                                   |
| `tailnet`   | userspace `tailscaled`, DERP over 443, DERP hostnames allowlisted, ephemeral key                                                                       | **spike M0-008**. A community report says UDP is blocked. |
| `wireguard` | `boringtun` in the sidecar + SOCKS for the harness; peer must be a hostname                                                                            | spike M0-008                                              |
| `gateway`   | per-org Fly Machine at a **public hostname** (`<org>-gw.fly.dev`), mTLS / CONNECT proxy. The gateway provides the private hop (tailnet/WireGuard/6PN). | guaranteed fallback                                       |
| client side | native apps reach a self-hosted hub over the OS VPN / embedded `libtailscale` on desktop                                                               |                                                           |

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

Single delegation is not enough. The hub runs a **durable flow engine**, so
multi-agent work survives Sprites sleeping, clients disconnecting and hub
restarts. A flow is a declarative graph of steps; each step that calls an agent
is an ordinary A2A task, so every managed or BYO agent can take part.

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

| Type               | What it does                                                                                                                                           |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `agent`            | Sends an A2A task to an agent chosen by id, label or skill. Supports a prompt template, attachments, structured output (JSON Schema) and a model hint. |
| `parallel` / `map` | Runs branches concurrently, or the same steps over a list (e.g. one review per lens). `join: all \| any \| quorum(n)`, with a concurrency limit.       |
| `loop`             | `while` / `until` CEL condition, `max_iterations` (required), and no-progress detection (same diff twice, same failing test set).                      |
| `branch`           | CEL `if` / `switch`.                                                                                                                                   |
| `run`              | A command in the flow's workspace via Sprites exec; exit code, stdout and stderr become step output.                                                   |
| `judge`            | An agent step whose output must match a verdict schema; it drives loops and branches.                                                                  |
| `human`            | An approval or form gate (AG-UI interrupt, push notification), with a timeout and `on_timeout` behaviour.                                              |
| `merge`            | Merges branch worktrees or Sprites back into the main workspace; conflicts go to an agent step or a human.                                             |
| `action`           | Built-ins (open PR, promote, notify), plus WASM plugin actions.                                                                                        |
| `ledger`           | The built-in **leader / worker / review** pattern (below).                                                                                             |
| `subflow`          | Calls another flow.                                                                                                                                    |

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

**AG-UI view**: a flow run is one thread.

- Steps → `StepStarted/StepFinished`; agent tasks → `Subagent*` (nested `Raw` per child event).
- Flow state (graph, iteration counters, budgets, ledger board) → `StateSnapshot/StateDelta`.
- Human gates → interrupts.

---

## 5. Milestones

Exit criteria are restated as ledger `*-review` items.

| #       | Milestone        | Exit criteria                                                                                                                                                                                                                                                                       |
| ------- | ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **M0**  | Revive + unblock | Cargo workspace + dev toolchain script. Sprites token (restricted). Protocol types pass 1.0 fixtures. Sprites client. Spikes answered: smoke (labels, checkpoint-stops-service), checkpoint fork, connector gateway, Sprite URL auth, Tasks keep-alive, networking.                 |
| **M1a** | Sidecar          | Claude via ACP served as an A2A agent from a Sprite service. Conformance kit passes on JSON-RPC (pinned commit). Durable across restart/cold wake. Path-routed auth. Keep-alive. Permission/cancel races covered. Checkpoint per turn.                                              |
| **M1b** | Client spike     | Gates written first. gpui-kit + Flutter task screen. Desktop decision (no devices). Mobile decision from on-device gates. ADRs.                                                                                                                                                     |
| **M2**  | Hub              | Ingest with exact dedup + reconciliation. AG-UI runs + subscription. OIDC, roles, audit. Secrets at rest. Notifications. Attachments. Titles/search. Retention. Ops. Deployed on Fly + MPG.                                                                                         |
| **M3**  | Client core      | Desktop then mobile: onboarding, composer, inbox, task view, approvals, diff, agents, notifications, a11y, offline queue. `cli e2e app` drives a task from the client.                                                                                                              |
| **M4**  | Workspaces       | Recipe template. Workspace from repo/issue. Hardening. Connector gateway. Timeline restore/fork/compare. Terminal/files/preview/lifecycle UIs. Auto-allow presets. Reaper. Exfiltration test. Managed agent launch.                                                                 |
| **M5**  | Fleet            | Loadouts + renderers for 6 harnesses + conformance matrix. MCP. Network profiles. Gateway. BYO E2E. Delegation. Usage. ACP v2 adapter (flagged).                                                                                                                                    |
| **M5c** | Flows            | Durable flow engine (agent/parallel/map/loop/branch/run/judge/human/merge/action/ledger/subflow steps). Workspace strategies. Fleet MCP delegation. Flows as A2A agents. Triggers. Budgets. Templates incl. ledger. Graph view + editor. E2E: ledger template completes a toy goal. |
| **M5b** | App plugins      | WASM host, two first-party plugins, catalog.                                                                                                                                                                                                                                        |
| **M6**  | Ship             | PR flow. Promote via Machines API. Zed bridge. Release workflows + signing. Retire v1.                                                                                                                                                                                              |
| Later   |                  | A2UI. Scheduled tasks (Sprites 2.0). MPG skill. Read-only web share page. Billing.                                                                                                                                                                                                  |

---

## 6. Repo plan

```
Cargo workspace (members = ["crates/*"])
crates/
  protocol/      a2a/, acp/, agui/, mapping/ (golden tests), schemas/ (our extensions)
  sprites/       generated client + hand-written checkpoint NDJSON + exec WS + undocumented fields
  loadout/       schema, merge, render/<harness>.rs
  netprofile/    public, wireguard, tailnet, gateway
  plugin-host/   wasmtime + WIT
  sidecar/       acp/, a2a/, bridge/, store/, keepalive/, auth/, preview/, checkpoint/
  acp-mock/      scriptable ACP v1 (later v2) agent for tests
  agui-fixture/  synthetic AG-UI streams
  hub/
  client-core/
  client-gpui/   (or apps/client-flutter via flutter_rust_bridge)
  acp-bridge/    local stdio ↔ hub bridge for Zed
  flow/          flow schema, CEL, durable engine, step runners, templates
  fleet-mcp/     MCP server exposing delegation tools to agents
  cli/           smoke, spikes, deploy, template, conformance, e2e
gateway/         egress gateway image
spikes/          gpui/, flutter/, bench/
scripts/         setup-dev.sh, pg-test.sh, tck.sh
manifest/        → default Loadout
```

Retired in M6, after a `v1-final` tag: the Expo app, the TypeScript control plane
and packages, the idle scheduler, the single-worker logic, the custom webhook
protocol, `USER_TOKEN`, and the auth-file upload.

---

## 7. Open questions / risks

1. **Sprites auth**: we need a restricted Sprites org token (M0-001). Fly tokens are out.
2. **gpui-kit mobile**: experimental. The gates + default-to-hybrid rule contain the risk.
3. **Cross-Sprite fork**: not in the API. Copy-based fork is slow; M0-010 measures it.
4. **Model credentials**: whether Sprites has a native Anthropic connector is unverified (custom_api is the fallback). Subscription-login harnesses can't be managed.
5. **Three-protocol mapping**: §1.1 as executable golden tests; raw payloads kept in the event log.
6. **Community Rust SDKs** (A2A, AG-UI): wrapped behind our traits and checked against 1.0 fixtures.
7. **Harness parity**: adapters differ (e.g. opencode's ACP reportedly auto-approves in some builds; pi adapters are community-maintained, with a changing package scope). The conformance matrix surfaces it.
8. **Approval latency/cost**: holding a Sprite awake for pending approvals costs compute; bounded by the approval TTL and auto-allow presets.
9. **Sprites limits & cost**: running/warm limits, create rate, checkpoint storage (no API delete). Admission control, reaper, retention, budgets.
10. **VPN inside Sprites**: may be impossible; the public-hostname gateway is the fallback.
11. **Plugin & supply-chain security**: §4.8, pinned lockfiles, SBOM.
12. **ACP v2 / remote transport**: draft. The `TurnEvent` layer and flagged adapter limit churn.
13. **Runaway multi-agent cost / loops**: required `max_iterations`, no-progress detection, budgets checked before each step, depth/fan-out limits on agent-initiated delegation, cascade cancel.
14. **Container limits for the ledger run**: no macOS, devices or Docker daemon; 4 vCPU. Human items plus CI cover these.
15. Evaluated and set aside: Tov, Expo/RN, Tauri, Compose Multiplatform.
