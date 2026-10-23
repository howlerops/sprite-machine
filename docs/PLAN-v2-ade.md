# sprite-machine v2 — an A2A Agent Development Environment on Sprites

> Status: plan / RFC · 2026-10-23
> Goal: a new app (iOS, Android, web, desktop) where you connect **any** agent fleet
> over the **A2A protocol** and drive it like Claude Code Remote: tasks, diffs,
> approvals, terminals, previews, checkpoints. Managed agents run on **Sprites**;
> bring-your-own agents connect by URL.

---

## 1. Why a rewrite, and what changed upstream

v1 (this repo) is a single-user, Claude-Code-centric orchestrator: one shared
"worker" Sprite, a custom sidecar webhook protocol, SQLite on one Fly volume, and an
Expo app. It works, but three things moved under it:

| Upstream change | What it means for us |
| --- | --- |
| **Sprites Block Device (SBD)** (new Sprites since 10/6). Disk is a bucket exposed over NBD, formatted as ext4. Checkpoints are versioned block snapshots of the whole volume. Restore rewinds the volume in place, which costs about a restart. Every block read back is verified. | Checkpoints are now reliable and cheap enough to take **one per agent turn**. "Undo turn", "fork from here" and "replay" become first-class ADE features. The v1 "one shared worker" workaround is no longer needed: use **one Sprite per workspace**. Existing pre-10/6 Sprites need the storage upgrade (docs.fly.io/sprites/sprite-maintenance#storage-upgrades). |
| **Sprites API 0.1.12** (`api.sprites.dev/openapi.json`) adds: **services** (`PUT /v1/sprites/{name}/services/{svc}` with `http_port`, `needs`, logs), **connectors** (`/v1/oauth/connections`: OAuth, API-key, custom API, and managed/provisioned keys with usage and balance), **policies** (network allow/deny by domain, privileges, memory resources), **labels**, **usage**, the fs API, and exec sessions (list/kill). | The sidecar becomes a Sprite **service**, which replaces v1's fragile exec-based worker startup. LLM and GitHub credentials live in **connectors** instead of on disk. Each agent gets its own **network/privileges policy**. Cost dashboards come from `/v1/usage`. |
| **`mcp.fly.io`** ("Feng Shui for Robots"). One MCP server covers Sprites and Fly apps. Authorization is a browser flow scoped to an org and selected apps. Agents can manage Machines, volumes and secrets on the apps you approve. Sprites 2.0 schedules are coming. | Every managed agent gets `mcp.fly.io` wired in, giving a native **"promote sandbox → production Fly app"** flow. Scheduled tasks wait for Sprites 2.0. |
| **A2A 1.0** (Linux Foundation). Agent Card at a well-known URI. Bindings: JSON-RPC, gRPC and HTTP+JSON. Operations: `SendMessage`, `SendStreamingMessage`, `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, push-notification config CRUD, `GetExtendedAgentCard`. States include `INPUT_REQUIRED` / `AUTH_REQUIRED`. Extensions are negotiated with the `A2A-Extensions` header. | A2A is the *only* wire protocol between the hub and agents. Claude Code, Codex, pi, Gemini CLI and opencode are just A2A agents behind our sidecar. Any third-party A2A agent plugs in with no code. |

---

## 2. Product shape

**Core nouns**

- **Fleet**: your set of connected agents (managed + BYO), grouped by labels.
- **Agent**: anything with an A2A Agent Card. Managed agents also own a Sprite.
- **Workspace**: repo + branch + Sprite (+ checkpoint lineage). One workspace can host several agents.
- **Task**: an A2A task. Its timeline is the A2A stream (status updates + artifacts), annotated with checkpoints.
- **Artifact**: diffs, files, test reports, PR links, preview URLs.

**Must-have ADE surfaces (parity with Claude Code Remote, plus fleet features)**

1. Inbox / task list across the whole fleet: live status, needs-input badges, push notifications.
2. Task view: streamed transcript, tool calls, **approval cards** (INPUT_REQUIRED), diff viewer, artifacts, cost.
3. Checkpoint timeline per workspace: restore, **fork into a new workspace**, compare.
4. Terminal (Sprites exec WS), file explorer/editor (fs API), live preview (Sprite URL / service `http_port`).
5. Agent registry: add by URL (fetch the card), or "launch managed agent" (pick a harness + model + connector + policy).
6. Multi-agent: hand a task to another agent; a planner agent fans out to sub-agents (A2A agent→agent through the hub).
7. Ship: open a PR (GitHub connector), or promote to a Fly app via `mcp.fly.io`.

**Non-goals for v2.0**: hosting third-party agents' compute, a marketplace, or a
full IDE (we link out to VS Code / Zed over SSH-into-Sprite for heavy editing).

---

## 3. Architecture

```
 ┌─────────────── Clients (one codebase) ───────────────┐
 │ Expo SDK 55: iOS · Android · Web (react-native-web)  │
 │ Desktop: Tauri 2 shell around the web build          │
 └──────────────┬───────────────────────────────────────┘
                │ HTTPS + SSE/WS  (hub API, OIDC session)
 ┌──────────────▼───────────────────────────────────────┐
 │ HUB  (Fly app, Machines, ≥2 regions)                 │
 │  • A2A client to every agent (JSON-RPC + REST)       │
 │  • A2A push-notification receiver (webhooks)         │
 │  • Task/event store → Fly Managed Postgres (MPG)     │
 │  • Fan-out bus → clients (SSE), Expo/APNs push       │
 │  • Agent registry, card cache/verification          │
 │  • Sprites orchestrator (create/label/policy/svc/ckpt)│
 │  • Exec/fs proxy for terminal + file explorer        │
 │  • Optional: hub *is itself* an A2A agent ("router") │
 └───────┬─────────────────────────────┬────────────────┘
         │ A2A                          │ A2A
 ┌───────▼──────────────┐      ┌────────▼───────────────┐
 │ Managed agent        │      │ BYO agent              │
 │ Sprite per workspace │      │ any A2A server         │
 │  service: a2a-sidecar│      │ (LangGraph, ADK,       │
 │   ├ claude / codex / │      │  CrewAI, custom…)      │
 │   │ pi / gemini /    │      └────────────────────────┘
 │   │ opencode adapters│
 │   ├ ckpt per turn    │──── Sprites API (checkpoints, fs, exec)
 │   └ mcp.fly.io       │──── connectors (LLM keys, GitHub)
 │  policy: net/priv/mem│
 └──────────────────────┘
```

### 3.1 A2A sidecar (`packages/a2a-sidecar`, replaces `packages/sprite-agent`)

- Runs as a Sprite **service** (`needs`, `http_port: 8080`), exposed on the Sprite URL with `url_settings.auth = "sprite"`, so the hub calls it with a scoped token. No more `auth: public` plus our own bearer.
- Serves `/.well-known/agent-card.json` (signed). The card lists skills per harness, `capabilities.streaming = true`, `pushNotifications = true`, and our extensions.
- Maps each harness's event stream to A2A:
  - assistant text → `Message` parts; tool use/result → `DataPart` with our `ade/tool` schema
  - permission prompt → `TASK_STATE_INPUT_REQUIRED` with an `ade/approval` DataPart; the reply is a `SendMessage` on the same task
  - missing credential → `TASK_STATE_AUTH_REQUIRED` pointing at a connector OAuth URL
  - file edits → `TaskArtifactUpdateEvent` with a unified diff (`text/x-diff`)
  - end of turn → POST `/checkpoint` with comment `task:<id> turn:<n>`, then emit an `ade/checkpoint` artifact
- Reuses the v1 adapters (`claude-cli`, `codex-cli`, `pi-cli`, `jsonl-subprocess`) almost as-is; only the output side changes.
- Built on `@a2a-js/sdk` (check its 1.0 support first; if it lags, write a thin 1.0 JSON-RPC server, because the surface is small).

### 3.2 ADE extensions (A2A extension URIs we publish)

| URI | Purpose |
| --- | --- |
| `https://sprite-machine.dev/ext/workspace/v1` | repo, branch, cwd, sprite name, checkpoint id carried in message metadata |
| `…/ext/approval/v1` | structured tool-approval request/response (allow once / always / deny + edit) |
| `…/ext/tool/v1` | tool call/result rendering (name, input, output, duration) |
| `…/ext/checkpoint/v1` | checkpoint created / restored events, plus a fork request |
| `…/ext/terminal/v1` | optional: agent advertises an exec endpoint for a live shell |

All extensions are **optional**. BYO agents without them still render as plain
chat + artifacts, and the UI degrades gracefully.

### 3.3 Hub (`apps/hub`, evolves from `apps/control-plane`)

- Keep Hono and zod. Replace SQLite with **Postgres on Fly MPG** (the v1 schema was written to be PG-compatible). Tables: `users, orgs, agents, agent_cards, workspaces, sprites, tasks, task_events(seq), artifacts, checkpoints, push_targets, connectors_ref, usage_daily`.
- `task_events` is an append-only log of raw A2A `StreamResponse`s. Clients resume with `?after=seq`, so the v1 "reconcile missed sidecar events" logic becomes generic.
- Ingest uses both paths: a live `SubscribeToTask` stream while a client is watching, and push-notification webhooks otherwise, so Sprites can sleep and nothing is lost.
- Auth: OIDC (GitHub / Google) for humans. The v1 single bearer `USER_TOKEN` goes away. Per-org Sprites token stored encrypted. Agents authenticate to the hub's webhook with per-task push tokens (A2A `AuthenticationInfo`).
- Idle handling: delete the v1 idle scheduler, because Sprites sleep/wake natively and the hub never needs to keep them awake.

### 3.4 Sprites orchestration (`packages/sprites-client`, regenerated)

- Generate types from `openapi.json` (0.1.12). Drop the v1 single-worker assumptions in `fly.ts`.
- **Template**: build a "golden" Sprite once (toolchains + harness CLIs + sidecar), checkpoint it, and create workspaces by cloning/restoring it. Rebuild in CI on harness version bumps.
- Per workspace: `labels = {fleet, workspace, harness}`; network policy (default-deny plus allowlist for package registries, GitHub and the model gateway); privileges policy; memory policy.
- Credentials: LLM keys and GitHub use **connectors**. Use `provision` for managed keys with spend telemetry and `api_key` / OAuth for BYO. Nothing secret is written to the Sprite disk. This also replaces v1's "auth JSON file upload".

### 3.5 Clients (`apps/client`, evolves from `apps/mobile`)

- Keep Expo SDK 55 + expo-router + gluestack/nativewind. The web build already exists (`dist-web`).
- Desktop: **Tauri 2** wrapping the web export, which gives native notifications, a tray "needs input" badge, and deep links. It is cheaper than Electron and keeps one UI codebase.
- New screens: Fleet, Agent detail (card, skills, health, cost), Workspace (checkpoint timeline, terminal, files, preview), Task (stream + approvals + diff), Inbox.
- Terminal: xterm.js on web/desktop, a WebView-hosted xterm on native.

---

## 4. Milestones

| # | Milestone | Exit criteria |
| --- | --- | --- |
| **M0** | Revive + unblock (≈2 days) | Branch `v2`. Mint a working **Sprites API token**: the provided `FLY_TOKEN` returns 401 from `api.sprites.dev`, so we need either a Sprites org token or Fly token exchange (see §6). Migrate any old Sprites to SBD. Regenerate the client from OpenAPI. CI green. |
| **M1** | A2A sidecar, one harness | The Claude Code adapter is served as an A2A agent via a Sprite service. It passes the A2A TCK (conformance kit) for JSON-RPC: send, stream, get, cancel, subscribe, push. Checkpoint per turn. |
| **M2** | Hub on A2A + Postgres | Register an agent by URL. Create/stream/cancel tasks. Event log with resume. Push-webhook ingest. OIDC login. Deployed to Fly. |
| **M3** | Client ADE core | Inbox, task view with approvals + diff, agent registry, on iOS/web. Expo push for INPUT_REQUIRED. |
| **M4** | Workspaces | One Sprite per workspace from the template. Checkpoint timeline with restore/fork. Terminal + files + preview. Policies and connectors wired up. |
| **M5** | Fleet | Codex/pi/Gemini/opencode adapters. A BYO third-party A2A agent works end-to-end. Agent→agent delegation through the hub. Usage/cost dashboard from `/v1/usage` + connector usage. |
| **M6** | Ship & desktop | GitHub PR flow. "Promote to Fly app" via `mcp.fly.io`. Tauri desktop builds. EAS store builds. |
| **Later** | | Scheduled tasks (Sprites 2.0), MPG skill for agents, signed-card trust UI, multi-tenant orgs/billing. |

---

## 5. Repo plan

```
apps/
  hub/            ← from apps/control-plane (A2A client, PG, OIDC)
  client/         ← from apps/mobile (+ src-tauri/ for desktop)
  cli/            ← template build, provision, token helpers
packages/
  a2a-sidecar/    ← from packages/sprite-agent (adapters kept)
  a2a-ext/        ← extension schemas (zod) shared by hub/sidecar/client
  sprites-client/ ← regenerated from openapi.json
  shared/
manifest/         ← keep: harness settings baked into the template
```

Delete: the idle scheduler, the single-worker logic, the custom webhook protocol,
`USER_TOKEN` auth, and the auth-file upload.

---

## 6. Open questions / risks

1. **Sprites auth**: `FLY_TOKEN` is rejected by `api.sprites.dev` (401). It is probably a Fly API/deploy token rather than a Sprites token. Options: create a Sprites token in the Sprites dashboard / `sprite` CLI, or use the Fly→Sprites token exchange if one exists. This blocks M0.
2. **A2A JS SDK 1.0 readiness**: if `@a2a-js/sdk` is still on 0.3, run our own thin 1.0 binding and send `A2A-Version: 1.0`.
3. **Interactive approvals over A2A**: INPUT_REQUIRED round-trips add latency to every tool prompt. Mitigate with per-workspace policy presets (auto-allow reads/tests).
4. **Single-user vs multi-tenant** for v2.0. Recommendation: single-org, multi-user first, with schema ready for tenants.
5. **Desktop**: Tauri (recommended) vs Electron, in case we need Node-native bits.
6. **Cost**: one Sprite per workspace. Mitigated by native sleep plus memory policy, and visible via usage API.
