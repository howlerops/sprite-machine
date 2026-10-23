# sprite-machine v2 — a multi-protocol Agent Development Environment on Sprites

> Status: plan / RFC · rev 2 · 2026-10-23
> Goal: a new **native app for mobile and desktop** (iOS, Android, macOS, Windows,
> Linux; no web at launch) where you connect **any** agent fleet and drive it like
> Claude Code Remote: tasks, diffs, approvals, terminals, previews, checkpoints.
> Managed agents run on **Sprites**. Bring-your-own agents connect over **A2A**.

---

## 1. Protocol stack: one protocol per boundary

The design rule: **each boundary speaks the open standard built for it**. We
translate between them in exactly two places, the sidecar and the hub.

| Boundary                                                                        | Protocol                                                                                                               | Why this one                                                                                                                                                                                                                                                                                                                                          |
| ------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Sidecar ↔ coding harness (Claude Code, Codex, Gemini CLI, Goose, opencode, pi…) | **ACP — Agent Client Protocol** (Zed). JSON-RPC over stdio. v1 is stable; v2 is in draft. TS and Rust SDKs are at 1.0. | Built for agent ↔ editor/IDE. Already models what an ADE needs: sessions, prompt turns, tool calls, **diffs**, **permission requests**, **terminals**, fs, agent plans, modes, slash commands, elicitation, session list/resume/fork/close, usage. There's an **ACP Registry** of installable agents. It replaces our hand-written v1 JSONL adapters. |
| Hub ↔ agents (managed _and_ BYO), agent ↔ agent                                 | **A2A 1.0** (Linux Foundation). JSON-RPC, gRPC and HTTP+JSON bindings.                                                 | Built for networked agent ↔ agent work: Agent Cards, tasks, streaming, push notifications for agents that sleep, `INPUT_REQUIRED` / `AUTH_REQUIRED`, extensions. The fleet-level standard, so any third-party agent plugs in.                                                                                                                         |
| Hub ↔ client UI                                                                 | **AG-UI** (ag-ui-protocol; from CopilotKit, LangChain and CrewAI). Event stream over HTTP (SSE) or WebSocket.          | Built for agent ↔ user-facing app. Typed events for text, tool calls, reasoning, **state snapshot/delta**, **activity**, **subagents**, **interrupts** (pause/approve/edit/resume). **Rust and Dart SDKs exist**, matching both client candidates.                                                                                                    |
| Agent-generated widgets (optional)                                              | **A2UI** (declarative generative-UI spec, carried over AG-UI)                                                          | Lets agents propose forms, pickers and cards that the client validates and renders natively. Phase 2.                                                                                                                                                                                                                                                 |
| Agent ↔ tools / infra                                                           | **MCP**, including **`mcp.fly.io`**                                                                                    | Tools, plus "promote sandbox → production Fly app".                                                                                                                                                                                                                                                                                                   |
| Desktop app ↔ Zed (bonus)                                                       | **ACP**, with our hub as the agent side                                                                                | "Open this workspace's agent in Zed": Zed talks ACP to an ACP proxy that forwards to the hub.                                                                                                                                                                                                                                                         |

```
 Harness CLIs ──ACP(stdio)── a2a-sidecar ──A2A── HUB ──AG-UI(SSE/WS)── Native clients
 (claude-agent-acp,          (in Sprite)          │  ▲                 (gpui-kit or Flutter)
  codex-acp, gemini, goose…)                      │  └─A2A── BYO agents (LangGraph, ADK, CrewAI…)
                                                  └─ACP proxy── Zed / other ACP editors (bonus)
```

### 1.1 Event mapping (the core of the system)

| ACP (harness → sidecar)              | A2A (sidecar → hub)                                      | AG-UI (hub → client)                                                       |
| ------------------------------------ | -------------------------------------------------------- | -------------------------------------------------------------------------- |
| `session/new`, `session/prompt`      | `SendStreamingMessage` (new or existing task)            | `RunStarted`                                                               |
| `session/update` agent message chunk | `TaskStatusUpdateEvent` with message `TextPart`          | `TextMessageStart/Content/End`                                             |
| agent thought chunk                  | message `DataPart` (`ext/reasoning`)                     | `Reasoning*`                                                               |
| tool call / tool call update         | `DataPart` (`ext/tool`)                                  | `ToolCallStart/Args/End/Result`                                            |
| tool call content: diff              | `TaskArtifactUpdateEvent` (`text/x-diff`)                | `ActivitySnapshot` (diff card) + `StateDelta` (workspace files)            |
| agent plan                           | `DataPart` (`ext/plan`)                                  | `ActivitySnapshot/Delta` (plan)                                            |
| `session/request_permission`         | state `TASK_STATE_INPUT_REQUIRED` + `ext/approval`       | `RunFinished{outcome: interrupt}` → client resumes a new run with `resume` |
| elicitation                          | `INPUT_REQUIRED` + `ext/elicitation`                     | interrupt (or an A2UI form, phase 2)                                       |
| auth required                        | `TASK_STATE_AUTH_REQUIRED` (Sprites connector OAuth URL) | interrupt (auth)                                                           |
| `terminal/*`                         | `ext/terminal` (exec session id)                         | `Custom` → opens a live terminal pane                                      |
| usage update                         | task metadata (`ext/usage`)                              | `StateDelta` (cost/context meters)                                         |
| sub-agent session                    | child A2A task (via hub)                                 | `SubagentStarted/Finished/Error`                                           |
| end of turn (+ our checkpoint)       | `ext/checkpoint` artifact                                | `ActivitySnapshot` (timeline)                                              |
| `session/cancel`                     | `CancelTask`                                             | client action → `RunFinished` / `RunError`                                 |

Everything not covered by a standard goes in **our A2A extensions**:
`ext/workspace`, `ext/tool`, `ext/plan`, `ext/approval`, `ext/elicitation`,
`ext/terminal`, `ext/usage`, `ext/checkpoint`, `ext/reasoning`. They live under
`https://sprite-machine.dev/ext/<name>/v1` and are negotiated with `A2A-Extensions`.
In practice they are mostly a thin encapsulation of ACP's own types. BYO agents
without them render as plain chat + artifacts.

---

## 2. What changed upstream (and why v1 gets rebuilt)

v1 (this repo) is single-user and Claude-Code-centric: one shared "worker" Sprite,
a custom sidecar webhook protocol, SQLite on one Fly volume, and an Expo app.

| Upstream change                                                                                                                                                                                                                | Impact                                                                                                                                                                                                             |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Sprites Block Device (SBD)** (Sprites created since 10/6): the disk is a bucket over NBD formatted as ext4; checkpoints are versioned block snapshots; restore costs about a restart; every block read is verified.          | **Checkpoint per agent turn**: undo turn, fork from here. **One Sprite per workspace** replaces the shared worker. Old Sprites need the storage upgrade (docs.fly.io/sprites/sprite-maintenance#storage-upgrades). |
| **Sprites API 0.1.12**: services (`http_port`, `needs`, logs), **connectors** (OAuth, API-key, custom API and managed keys, with usage and balance), network/privileges/memory **policies**, labels, usage, fs, exec sessions. | The sidecar runs as a Sprite **service** (fixes v1 worker-startup flakiness). Credentials live in connectors, never on disk. Per-agent sandbox policy. Cost dashboards.                                            |
| **`mcp.fly.io`**: one MCP for Sprites and Fly apps, browser-authorized and app-scoped. Sprites 2.0 schedules are coming.                                                                                                       | "Promote to production" flow. Scheduled tasks later.                                                                                                                                                               |
| **ACP** matured (registry, SDK 1.0, session resume/fork, remote transport in progress).                                                                                                                                        | Harness integration becomes "install from the ACP Registry".                                                                                                                                                       |
| **A2A 1.0**, **AG-UI 1.0**                                                                                                                                                                                                     | Standard wire formats at the fleet and UI boundaries.                                                                                                                                                              |

---

## 3. Product shape

**Nouns**:

- **Fleet**: agents grouped by labels.
- **Agent**: has an A2A Agent Card. Managed agents also have an ACP harness + Sprite.
- **Workspace**: repo + branch + Sprite + checkpoint lineage.
- **Task**: A2A task ↔ AG-UI runs.
- **Artifact**: diffs, files, reports, PRs, preview URLs.

**Surfaces**:

1. **Inbox** across the fleet: needs-input badges and push notifications.
2. **Task view**: streamed transcript, reasoning, tool calls, approval cards, plan, diff viewer, cost and context meters.
3. **Checkpoint timeline**: restore, **fork into a new workspace**, compare.
4. **Workspace tools**: terminal (Sprites exec), file tree and editor (fs API), live preview (Sprite URL / service port).
5. **Agent registry**:
   - add a BYO agent by URL (we fetch its A2A card), or
   - launch a managed agent: pick an ACP agent from the registry + model connector + policy.
6. **Multi-agent**: delegate tasks, planner → sub-agents (AG-UI `Subagent*` events).
7. **Ship**: open a GitHub PR, or promote to Fly via `mcp.fly.io`.
8. **Open in Zed** (desktop) through the ACP proxy.

**Non-goals for v2.0**: a web client (a read-only shared task page may come later),
hosting third-party agents' compute, a marketplace.

---

## 4. Architecture

### 4.1 Sidecar (`crates/` or `packages/sidecar`), inside each Sprite

- Runs as a Sprite **service** on `http_port 8080`, with URL auth `sprite`.
- **ACP client**: spawns the chosen harness through its ACP adapter (stdio). It implements the client-side ACP methods: fs read/write against the workspace, terminals mapped to local PTYs, and permission requests.
- **A2A server**: serves a signed Agent Card (skills = the installed ACP agents, `streaming` and `pushNotifications` enabled, our extensions). It applies the §1.1 mapping and supports push webhooks to the hub so the Sprite can sleep between turns.
- At end of turn it calls the Sprites checkpoint API with comment `task:<id> turn:<n>`.
- **Language**: Rust (ACP Rust SDK 1.0; a small A2A server), which gives a single static binary in the Sprite template. TypeScript with the ACP TS SDK is the fallback.

### 4.2 Hub (`apps/hub`, evolves from `apps/control-plane`)

**What it does**:

- **A2A client** to all agents: live `SubscribeToTask` streams plus push-webhook ingest.
- **AG-UI server** to clients: one run stream per task, multiplexed inbox stream.
- Append-only `task_events` log of raw A2A events. AG-UI is a deterministic projection of that log, so reconnects replay from a `seq` and no events are lost.
- Projects workspace and agent state into AG-UI `StateSnapshot`/`StateDelta` (JSON Patch).
- **Sprites orchestrator**:
  - creates Sprites from the template checkpoint;
  - sets labels, policies and connectors;
  - handles checkpoint/restore/fork;
  - proxies exec and fs for the terminal and file tree.
- **ACP proxy endpoint**: exposes any fleet agent to ACP editors (Zed), using ACP's remote transport (Streamable HTTP/WS) once it stabilizes, or a tiny local stdio↔WS bridge until then.
- Push notifications (APNs/FCM) on `INPUT_REQUIRED` and task completion.

**Stack**:

- Postgres on Fly MPG, replacing SQLite (the v1 schema was written to be PG-compatible).
- OIDC login, replacing the v1 single `USER_TOKEN`.
- Per-org Sprites token stored encrypted.
- Language: TypeScript (Hono, A2A JS, AG-UI integrations) or Rust if the sidecar goes Rust and we want one toolchain. Decide in M0.

### 4.3 Sprites orchestration

- A typed client generated from `api.sprites.dev/openapi.json`.
- **Golden template**: toolchains + ACP agents (from the registry) + sidecar, then a checkpoint. Rebuilt in CI on version bumps.
- **Per workspace**:
  - labels `{fleet, workspace, agent}`;
  - default-deny **network policy** with allowlists (package registries, GitHub, model gateway);
  - privileges and memory policies.
- **Credentials only via connectors**: managed keys with spend telemetry, or BYO API keys/OAuth. This replaces v1's auth-file upload.

### 4.4 Native clients (mobile + desktop, no web)

**Primary candidate: gpui-kit** (Longbridge; Rust on GPUI, Zed's UI framework; Apache-2.0; v0.8).

- Desktop (macOS/Windows/Linux) is production-grade.
- iOS/Android has been **experimental since v0.6.2**, via `longbridge/gpui-mobile`, with a native Swift/Android host embedding GPUI. Only the iOS simulator has been validated so far.
- A WebAssembly target exists if web returns later.
- It brings a Tree-sitter + LSP code editor, virtualized lists, dock layouts and Markdown.
- Terminal via `alacritty_terminal` (as Zed does). AG-UI via its **Rust SDK**.
- Zed's own ACP agent panel is a reference implementation for the task view.

**Fallback: Flutter** (AOT native on all five platforms, Impeller, small binaries).

- AG-UI via its **Dart SDK**.
- Terminal via `xterm.dart`.
- Weaker code and diff components.

**Hybrid option**: gpui-kit on desktop + Flutter on mobile, sharing a **Rust client core** through `flutter_rust_bridge`. The core holds the AG-UI client, local event store, diff parsing and sync.

**Decided by spike M1b** (§5). Rejected: Expo/React Native (weak desktop, no Linux) and Tauri (web view, no benefit without a web target).

### 4.5 Language decision: Rust end to end

**Decision**: one Cargo workspace for the sidecar, hub, client core and client UI.
TypeScript survives only where a harness forces it, and those run as separate
ACP subprocesses anyway (e.g. the npm-installed pi/opencode/Claude adapters).

| Factor             | Rust                                                                                                                            | TypeScript                                                             |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| gpui-kit client    | Native. GPUI _is_ Rust, so it shares types with the client core.                                                                | Not possible                                                           |
| Flutter fallback   | Rust client core via `flutter_rust_bridge`, so the work isn't wasted                                                            | Would need Dart duplicates                                             |
| ACP                | **Official Rust SDK 1.0** (Zed's own language and implementation)                                                               | Official TS SDK 1.0                                                    |
| A2A 1.0            | Community crates claim 1.0 (`ra2a`, `a2a-rs`, `a2a-rust`). Or codegen from the official `.proto` (prost/tonic) and JSON Schema. | Official JS SDK                                                        |
| AG-UI              | Rust SDK listed (community)                                                                                                     | Official TS core                                                       |
| Sidecar in Sprites | Single static binary, few MB of RAM (we pay for reserved memory), fast cold start after wake                                    | Node runtime, ~50–100 MB RSS                                           |
| Hub                | tokio/axum handle many long-lived SSE/WS streams cheaply; one set of protocol types shared with sidecar and client              | Faster to iterate on CRUD                                              |
| Plugins            | **wasmtime + WIT components**, the model Zed uses for its extensions                                                            | V8 isolates / worker sandboxes, weaker                                 |
| Networking/VPN     | `boringtun` (userspace WireGuard), `libtailscale` FFI, rustls                                                                   | Mostly shells out                                                      |
| Terminal           | `alacritty_terminal` shared by client and sidecar                                                                               | xterm.js only                                                          |
| Costs              | Slower iteration, compile times, some SDKs community-maintained                                                                 | Rewrites types per layer, two languages anyway once the client is Rust |

**Mitigations**:

- One `protocol` crate holds generated A2A/AG-UI/ACP types plus our extensions, with golden mapping tests.
- Pin the community crates behind our own traits, so swapping a crate costs one adapter.
- The Sprites client is generated from OpenAPI (`progenitor`).

### 4.6 Extensibility: harnesses, skills, MCP, plugins

Everything an agent can be extended with is one declarative, versioned
**Loadout**. It generalizes v1's `manifest/` (claude/codex/pi settings,
plugins.json). Loadouts attach at org, workspace or task level and are merged in
that order.

```yaml
# loadout.yaml
harnesses: # any ACP agent; ACP Registry id or command
  - claude # via claude-agent-acp
  - opencode # native: `opencode acp`
  - pi # via a pi ACP adapter (pi-acp / pi-acpinator), pinned
  - codex, gemini, goose
skills: # Agent Skills folders (SKILL.md), from git/registry/local
  - github:org/skills//frontend-design@v3
mcp: # MCP servers; stdio inside the Sprite or remote HTTP
  - name: linear
    url: https://mcp.linear.app/mcp
    auth: connector:linear # Sprites connector; no token on disk
  - name: fly
    url: https://mcp.fly.io/
commands: [...] # slash commands / prompts
subagents: [...] # agent definitions (e.g. v1 codex agents/*.toml)
hooks: [...] # pre/post tool hooks, mapped per harness where supported
harness_plugins: # native plugin systems, passed through untouched
  claude: [marketplace:org/plugins#review]
  pi: [pi.dev package names]
network: profile:corp-tailnet # §4.7
policy: { auto_allow: [read, test], deny_paths: [...] }
```

**How a Loadout is applied**:

- **Harness install**: baked into the golden template where common, otherwise installed lazily on first use (cached by checkpoint).
- **MCP**: passed per session through ACP `session/new` → `mcpServers`, so it works the same for every ACP harness. Also written into native configs for harnesses that ignore ACP-provided MCP.
- **Skills, commands, subagents, hooks**: a **renderer per harness** writes them in that harness's native layout (`.claude/skills`, `.codex/`, opencode/pi config dirs). This is v1's `sync-settings`, generalized and tested per harness.
- **Secrets**: only Sprites connectors (OAuth/API-key/custom API). The sidecar injects them as short-lived env vars or proxy auth. Nothing is persisted in checkpoints.

**App plugins** (extend _our_ product, not the agent):

- Sandboxed **WASM components** (wasmtime, WIT interfaces), Zed-extension style, loaded by the hub and the client.
- Capability-scoped hooks: custom AG-UI event renderers / A2UI component sets, task lifecycle hooks (e.g. post to Slack), new agent providers, new network profiles, Loadout sources.

**Registries**:

- ACP Registry for harnesses.
- MCP registry for servers.
- Git/URL sources for skills and plugins.
- An org-private catalog in the hub, with version pinning and an allowlist.

### 4.7 Networking and VPNs

Agents often need private resources: internal Git, package mirrors, staging DBs,
corp APIs. **Network Profiles** are hub objects, attached via the Loadout, that
compose with the Sprites network policy.

| Profile type | How                                                                                                                                                                      | Notes                                                                                                                                                    |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `public`     | Sprites network policy allowlist                                                                                                                                         | Default-deny + registries                                                                                                                                |
| `tailnet`    | `tailscaled` in userspace-networking mode inside the Sprite, authed with an ephemeral pre-auth key (from a connector)                                                    | A community report says Sprites network limits block Tailscale's UDP. It may work over DERP (TCP 443) with the DERP hosts allowlisted. **Verify in M0.** |
| `wireguard`  | `boringtun` (userspace WireGuard) in the sidecar, plus a SOCKS/HTTP proxy for the harness                                                                                | Generic: any WireGuard VPN, Headscale, NetBird                                                                                                           |
| `gateway`    | A **Fly Machine egress gateway** per org: joined to the tailnet/VPN or Fly's private network (6PN). The Sprite reaches it through the network-policy allowlist plus mTLS | Fallback when in-Sprite VPN is blocked. Also how agents reach Fly apps privately.                                                                        |
| client side  | The native apps reach a self-hosted hub or BYO agents over the OS VPN, or embedded `libtailscale` on desktop                                                             | For agents/hubs that live inside a private network                                                                                                       |

The UI shows each workspace's effective egress (profile + policy) and logs denied
domains, so "why can't my agent reach X" is answerable.

---

## 5. Milestones

| #         | Milestone                            | Exit criteria                                                                                                                                                                                                                                                                                                                                             |
| --------- | ------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **M0**    | Revive + unblock                     | Branch `v2`. Working **Sprites token** (§7.1). Migrate old Sprites to SBD. Cargo workspace + `protocol` crate. Generate the Sprites client. Verify the `tailnet` profile on a Sprite (§4.7). CI green.                                                                                                                                                    |
| **M1a**   | Sidecar                              | One ACP agent (Claude via its ACP adapter) is exposed as an A2A agent from a Sprite service. Passes the A2A conformance kit (send, stream, get, cancel, subscribe, push). Checkpoint per turn. Permission → `INPUT_REQUIRED` round-trip works.                                                                                                            |
| **M1b**   | **Client spike** (parallel, ~1 week) | The same task screen built in gpui-kit **and** Flutter: AG-UI stream with 10k events, 5k-line diff, terminal, approval interrupt. Measured on a **physical iPhone + mid-range Android** + one desktop for frame rate, input latency, memory, cold start, binary size, keyboard/IME, accessibility, and native push through the host. Pick a client stack. |
| **M2**    | Hub                                  | A2A ingest → event log → AG-UI projection. Register a BYO agent by URL. OIDC. Push notifications. Deployed on Fly + MPG.                                                                                                                                                                                                                                  |
| **M3**    | Client core                          | Inbox, task view (text, reasoning, tools, plan, diff, approvals), agent registry. Mobile + desktop.                                                                                                                                                                                                                                                       |
| **M4**    | Workspaces                           | Sprite per workspace from the template. Checkpoint timeline with restore/fork. Terminal, files, preview. Policies + connectors.                                                                                                                                                                                                                           |
| **M5**    | Fleet                                | **Loadouts** (§4.6): opencode, pi, Codex, Gemini CLI and Goose via ACP; skills, MCP and harness plugins rendered per harness; network profiles (§4.7). A third-party BYO A2A agent works end to end. Delegation + `Subagent*` UI. Usage/cost dashboard.                                                                                                   |
| **M6**    | Ship                                 | GitHub PR flow, promote to Fly via `mcp.fly.io`, "Open in Zed" (ACP proxy), store + desktop release builds.                                                                                                                                                                                                                                               |
| **M5b**   | App plugins                          | WASM plugin host (hub + client) with 2 first-party plugins (Slack notifier, custom renderer). Org-private catalog.                                                                                                                                                                                                                                        |
| **Later** |                                      | A2UI generative widgets. Scheduled tasks (Sprites 2.0). MPG skill. Read-only web share page. Multi-tenant billing.                                                                                                                                                                                                                                        |

---

## 6. Repo plan

```
Cargo workspace
crates/
  protocol/        A2A + AG-UI + ACP types, our extensions, golden mapping tests
  sprites/         Sprites API client (generated from openapi.json)
  loadout/         Loadout schema, merge, per-harness renderers
  netprofile/      tailnet / wireguard / gateway profiles
  plugin-host/     wasmtime + WIT interfaces
  sidecar/         ACP client + A2A server   ← replaces packages/sprite-agent
  hub/             A2A ingest, AG-UI server, orchestration  ← replaces apps/control-plane
  client-core/     AG-UI client, event store, diff, sync
  client-gpui/     desktop + mobile UI (or apps/client-flutter/ via flutter_rust_bridge)
  cli/             template build, provisioning, loadout tooling
gateway/           Fly Machine egress gateway image
manifest/          migrates into default Loadouts
```

Retired from v1: the Expo app, the idle scheduler, the single-worker logic, the
custom webhook protocol and JSONL adapters (replaced by ACP), `USER_TOKEN` auth,
the auth-file upload, and the TypeScript packages (replaced by Rust crates, §4.5).

---

## 7. Open questions / risks

1. **Sprites auth**. Sprites belong to Fly (`sprite login` uses Fly's browser login), but `api.sprites.dev` returned 401 for `Authorization: Bearer $FLY_TOKEN`. It is unverified whether a Fly **org token** (`fly tokens create org`, `FlyV1 fm2_…`) works with the `FlyV1` scheme. The documented path is a Sprites org token from `sprite org auth`, formatted `org-slug/org-id/token-id/token-value`. Store it as the environment secret `SPRITES_API_TOKEN`. This blocks M0.
2. **gpui-kit mobile maturity**: experimental, simulator-validated only, and depends on a fork (`gpui-pre-mobile`). M1b exists to retire this risk. The fallbacks are Flutter or the hybrid.
3. **Three protocols, lossy mapping**: keep the mapping table (§1.1) as executable golden tests in `protocol-ext`. Store raw ACP and A2A payloads in the event log so nothing is unrecoverable.
4. **ACP remote transport** is still in progress. The "Open in Zed" feature uses a local bridge until it stabilizes.
5. **Rust SDK maturity**: the A2A and AG-UI Rust crates are community-maintained. Wrap them behind our traits, or codegen from the official proto/JSON Schema. The surfaces are small enough to own.
6. **Harness parity**: ACP adapters differ in what they support. For example, opencode's ACP reportedly auto-approves permissions in some builds, and pi relies on community adapters (`pi-acp`, `pi-acpinator`, package scope changing). Run a per-harness conformance checklist in CI and show the gaps in the UI.
7. **VPN inside Sprites**: Sprites network limits may block Tailscale's UDP; the egress gateway is the guaranteed fallback.
8. **Plugin security**: WASM capabilities are deny-by-default. Harness-native plugins (Claude marketplace, pi packages) run unsandboxed inside the Sprite, which is acceptable only because the Sprite is the sandbox. Gate them with the org allowlist.
9. **Approval latency**: interrupts round-trip ACP → A2A → AG-UI → user. Mitigate with per-workspace auto-allow presets, mapped to ACP permission options.
10. **Tenancy**: start single-org, multi-user, with the schema ready for tenants.
11. **Cost**: one Sprite per workspace. Mitigated by native sleep, memory policy and the usage API.
12. Evaluated and set aside: **Tov** (v0.0.1 experimental language; maybe later for small binaries), **Expo/RN** and **Tauri** (see §4.4), **Compose Multiplatform** (JVM desktop footprint).
