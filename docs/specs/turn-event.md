# Spec: the TurnEvent envelope (v1)

> Status: **normative** · SPEC-001 · written by the ledger leader before fan-out.
> Plan: `docs/PLAN-v2-ade.md` §1, §1.1, §1.2. Schemas: `crates/protocol/schemas/turn-event/`.
> Workers implement this spec; they never change it. A change needs a leader edit here, in the schemas and in the examples together.

## 1. Purpose and scope

Inside the system we control there is exactly **one** event format: the TurnEvent
envelope. It flows:

- runner/sidecar → hub, inside tunnel `event` frames (`docs/specs/tunnel.md`);
- hub (Thread DO) → our clients (desktop app, mobile web page, TUI, native mobile), over the
  thread subscription WebSocket (§6);
- hub → stored event log (Thread DO SQLite).

**A2A and AG-UI never appear inside the system.** They exist only in the edge adapters
(`crates/a2a-edge`, `crates/agui-edge`, and the pure mapping functions in
`crates/protocol/src/mapping/`). Variant names are AG-UI-shaped so the AG-UI adapter is
mechanical, but field names are our own (`snake_case`).

## 2. Envelope

```json
{
  "v": 1,
  "seq": 42,
  "thread_id": "th_1",
  "task_id": "tk_1",
  "run_id": "run_1",
  "ts": "2026-10-27T22:00:03.000Z",
  "event": {
    "type": "TextMessageContent",
    "message_id": "msg_a1",
    "delta": "Adding the route now."
  },
  "raw": {
    "source": "acp",
    "payload": {
      "sessionUpdate": "agent_message_chunk",
      "content": { "type": "text", "text": "Adding the route now." }
    }
  }
}
```

| Field       | Type            | Required | Meaning                                                                                                                                                        |
| ----------- | --------------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `v`         | `1`             | yes      | Envelope version. A consumer rejects any other value.                                                                                                          |
| `seq`       | integer ≥ 0     | yes      | Per-thread sequence number (§3). `0` only on the producer side, before the Thread DO assigns it.                                                               |
| `thread_id` | Id              | yes      | Conversation (ACP `sessionId` ↔ A2A `contextId` ↔ AG-UI `threadId`). A flow run's thread id is the flow run id.                                                |
| `task_id`   | Id \| null      | yes      | One prompt turn (A2A task). `null` only for thread-scoped events (§4, column "Scope").                                                                         |
| `run_id`    | Id \| null      | yes      | One run inside the task (§5). `null` for task- and thread-scoped events.                                                                                       |
| `ts`        | RFC 3339 string | yes      | Producer time, UTC, millisecond precision (`2026-10-27T22:00:03.000Z`). Informational only: ordering is by `seq`, never by `ts`.                               |
| `event`     | object          | yes      | One variant from §4, discriminated by `type`.                                                                                                                  |
| `raw`       | object          | no       | `{source: "acp" \| "a2a" \| "agui" \| "flow" \| "hub", payload}`: the source payload this event was mapped from, kept verbatim (after redaction) for fidelity. |

**Ids** (`Id` in the schemas) match `^[A-Za-z0-9_:.\-]+$`, 1–128 chars, and are opaque.
Producers use a type prefix plus a ULID: `th_`, `tk_`, `run_`, `msg_`, `rsn_`, `tc_` (or the
ACP `toolCallId` verbatim when it matches the pattern; otherwise `tc_` + a hash of it),
`int_`, `cp_`, `sub_`, `mch_`, `usr_`, `dev_`, `agt_`. `activity_id` is a free string
(1–1024 chars) because it embeds a path.

**Unknown fields and variants**: the schemas are closed (`additionalProperties: false`)
so producers can be tested strictly. Consumers must still **ignore** (and not crash on)
an unknown `event.type`, so newer hubs can talk to older clients.

**Size**: a serialized envelope is at most **256 KiB**. A producer splits larger diff
or terminal content into consecutive `Activity` events (diff: `content.chunk = {index, last}`;
terminal: `mode: "append"`). `raw.payload` is dropped (not truncated) when keeping it would
exceed the limit.

**Redaction**: secret patterns are redacted (in `event` and `raw`) before the Thread DO
persists an envelope. Producers do not rely on redaction: they never put credentials in events.

## 3. `seq`: per thread, assigned by the Thread DO, gap-free

- Each thread has one sequence. The first persisted event has `seq = 1`; every later event
  is exactly the previous `seq + 1`. There are no gaps and no reuse, ever (deleting a
  thread deletes the whole log).
- **Only the Thread DO assigns `seq`**, at append time. Durable Objects are single-threaded,
  so assignment is a counter in the DO's SQLite, in the same transaction as the append.
- Producers (runner mapping, A2A edge, flow engine, hub routes) emit envelopes with
  `seq: 0`. `crates/protocol` provides a `ThreadSeq` stamper (next = last + 1) that the
  Thread DO uses, and that golden tests and in-memory stubs use to produce final envelopes.
- Delivery from a producer to the Thread DO is exactly-once by the transport (tunnel frame
  seq + ack, idempotency keys), **not** by envelope `seq`. The envelope `seq` is a property of
  the stored log only.
- A consumer that receives `seq = n + 2` after `n` has missed an event: it must resubscribe
  with `?after=n` (§6). Consumers deduplicate by `seq` (drop `seq ≤` the last applied one).

## 4. Variants

"Scope" says which ids the envelope carries:

- **run**: `task_id` and `run_id` both set;
- **task**: `task_id` set, `run_id` null;
- **thread**: both null.

The **ACP source** column names the ACP v1 message (`session/update` → `sessionUpdate`
value) it is mapped from (`crates/protocol/src/mapping/acp_turn*` (`AcpTurnMapper`, `SequenceValidator`), item M1A-006a). Every
variant has a schema in `events/<Type>.schema.json` and an example in `examples/<Type>.json`.

### 4.1 Lifecycle

**`UserMessage`** (scope: task). The prompt that opens a task. From ACP `session/prompt`
input (and `user_message_chunk` on `session/load` replay); A2A user `Message`. **Emitted by the
hub** when the task starts (docs/specs/client-api.md §6); the runner emits it only for prompts
typed locally on a runner-opened thread.

| Field        | Type          | Req | Notes                                                                                                |
| ------------ | ------------- | --- | ---------------------------------------------------------------------------------------------------- |
| `message_id` | Id            | ✓   |                                                                                                      |
| `parts`      | ContentPart[] | ✓   | ≥1. `{type:"text", text}` or `{type:"file", name, mime_type, url, size?, sha256?}` (https URL only). |
| `author`     | Actor         | ✓   | `{kind: user\|policy\|system\|agent, id, name?, device_id?}`                                         |

**`RunStarted`** (scope: run). ACP: the runner sends `session/prompt` (reason `prompt`),
or continues after an interrupt is resolved (reason `resume`, `parent_run_id` = the run
that ended with the interrupt), or re-runs a failed turn (reason `retry`).

| Field           | Type                            | Req |
| --------------- | ------------------------------- | --- |
| `reason`        | `prompt` \| `resume` \| `retry` | ✓   |
| `parent_run_id` | Id                              |     |
| `agent_id`      | Id                              |     |
| `machine_id`    | Id                              |     |

**`RunFinished`** (scope: run). ACP: `session/prompt` response with `stopReason`
`end_turn` / `max_tokens` / `max_turn_requests` / `refusal`; or the run pauses on interrupts.

| Field     | Type                                                                            | Req | Notes                                                                      |
| --------- | ------------------------------------------------------------------------------- | --- | -------------------------------------------------------------------------- |
| `outcome` | `{type:"success", stop_reason?}` \| `{type:"interrupt", interrupt_ids: Id[≥1]}` | ✓   | `interrupt` lists every `Interrupt` raised in this run that is still open. |

**`RunError`** (scope: run). ACP: `stopReason: cancelled` or a `session/cancel` (reason
`cancelled`); a harness crash, protocol error, approval TTL expiry or budget stop (reason `error`).

| Field     | Type                   | Req | Notes                                                           |
| --------- | ---------------------- | --- | --------------------------------------------------------------- |
| `reason`  | `cancelled` \| `error` | ✓   |                                                                 |
| `code`    | string                 |     | `expired`, `harness_exited`, `protocol`, `timeout`, `budget`, … |
| `message` | string                 | ✓   | Human-readable.                                                 |

### 4.2 Text and reasoning

ACP sends `agent_message_chunk` / `agent_thought_chunk` with a content block. The runner
opens a message on the first chunk after any other kind of update, appends text blocks as
`*Content`, and closes it on the next update of a different kind or at the end of the run.
Non-text blocks inside a message chunk (images, resources) produce no `*Content`
event in v1; the producer emits a `Raw{source:"acp"}` event carrying the chunk so nothing is lost
(rendered later via A2UI).

| Variant                   | ACP source            | Fields                                       |
| ------------------------- | --------------------- | -------------------------------------------- |
| `TextMessageStart`        | `agent_message_chunk` | `message_id` ✓, `role` ✓ = `"assistant"`     |
| `TextMessageContent`      | `agent_message_chunk` | `message_id` ✓, `delta` ✓ (non-empty string) |
| `TextMessageEnd`          | (next update / end)   | `message_id` ✓                               |
| `ReasoningMessageStart`   | `agent_thought_chunk` | `message_id` ✓, `role` ✓ = `"reasoning"`     |
| `ReasoningMessageContent` | `agent_thought_chunk` | `message_id` ✓, `delta` ✓ (non-empty string) |
| `ReasoningMessageEnd`     | (next update / end)   | `message_id` ✓                               |

All scope: run.

### 4.3 Tool calls (upsert by `tool_call_id`)

Consumers keep a map `tool_call_id → ToolCall`. ACP `tool_call` and `tool_call_update` map as follows:

| Variant          | ACP source                                                                                     | Fields                                                                                                                                                                                                                     | Consumer rule                                                                      |
| ---------------- | ---------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| `ToolCallStart`  | `tool_call`; `tool_call_update` changing title/kind/status (`pending`/`in_progress`)/locations | `tool_call_id` ✓, `title`, `kind` (ACP ToolKind: read, edit, delete, move, search, execute, think, fetch, switch_mode, other), `status` (`pending` \| `in_progress`), `locations` (`{path, line?}[]`), `parent_message_id` | **Upsert**: the first one creates the entry; later ones merge every field present. |
| `ToolCallArgs`   | `rawInput` on `tool_call` / `tool_call_update`                                                 | `tool_call_id` ✓, `raw_input` ✓ (any JSON)                                                                                                                                                                                 | Replaces the stored args.                                                          |
| `ToolCallResult` | `content` items of type `content`; `rawOutput`                                                 | `tool_call_id` ✓, `content` ✓ (ContentPart[]), `raw_output`                                                                                                                                                                | Replaces the stored result.                                                        |
| `ToolCallEnd`    | `tool_call_update` with status `completed` / `failed`                                          | `tool_call_id` ✓, `status` ✓ (`completed` \| `failed`)                                                                                                                                                                     | Closes the entry. Nothing may follow for this id.                                  |

ACP tool-call content of type `diff` becomes `Activity{diff}`; type `terminal` becomes
`Activity{terminal}` (§4.4), linked by `tool_call_id`. All scope: run.

### 4.4 `Activity` (diff, plan, terminal, policy)

One variant, keyed by `activity_id`, scope: run.

| Field           | Type                                       | Req                 | Notes                                                                                       |
| --------------- | ------------------------------------------ | ------------------- | ------------------------------------------------------------------------------------------- |
| `activity_id`   | string (1–1024)                            | ✓                   | Stable per activity: diff `"<tool_call_id>:<path>"`, plan `"plan"`, terminal `terminal_id`. |
| `activity_type` | `diff` \| `plan` \| `terminal` \| `policy` | ✓                   |                                                                                             |
| `mode`          | `snapshot` \| `append` \| `patch`          | ✓                   | `snapshot` replaces, `append` appends (terminal `output`, diff chunks), `patch` = RFC 6902. |
| `content`       | object (per type, below)                   | for snapshot/append |                                                                                             |
| `patch`         | JSON Patch                                 | for patch           |                                                                                             |

| `activity_type` | ACP source [v2]                                                                        | `content`                                                                                         |
| --------------- | -------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| `diff`          | tool-call content `{type:"diff", path, oldText, newText}` [v2 `changes` + `git_patch`] | `{tool_call_id?, path ✓, old_text (string \| null = new file), new_text ✓, chunk? {index, last}}` |
| `plan`          | `plan` (full list each time) [v2 `plan_update`]                                        | `{entries ✓: [{content, priority: high\|medium\|low, status: pending\|in_progress\|completed}]}`  |
| `terminal`      | client terminals (`terminal/create` output, tee'd) [v2 `terminal_output_chunk`]        | `{terminal_id ✓, tool_call_id?, command?, cwd?, output?, exit? {exit_code, signal}}`              |
| `policy`        | — (runner auto-allow presets, M4-008)                                                  | `{tool_call_id ✓, decision ✓: allow\|deny, rule ✓}`                                               |

A terminal starts with a `snapshot` (command, cwd), continues with `append` (`output`
chunks, UTF-8 lossy), and ends with a `snapshot` carrying `exit`. Because a snapshot replaces
the whole content, the closing snapshot repeats `terminal_id`, `command`, `cwd` and the full
`output` accumulated so far; it must still fit the 256 KiB envelope limit (§2).

### 4.5 Interrupts

**`Interrupt`** (scope: run). The run needs a human.

| Field             | Type                           | Req | Notes                                                                                                                |
| ----------------- | ------------------------------ | --- | -------------------------------------------------------------------------------------------------------------------- |
| `interrupt_id`    | Id                             | ✓   |                                                                                                                      |
| `kind`            | `approval` \| `form` \| `auth` | ✓   | approval ← ACP `session/request_permission`; form ← `elicitation/create` (form mode); auth ← elicitation (url mode). |
| `title`           | string                         | ✓   | approval: the tool call title.                                                                                       |
| `message`         | string                         |     |                                                                                                                      |
| `expires_at`      | Timestamp                      | ✓   | Approval TTL (default 10 min, plan §4.1).                                                                            |
| `tool_call_id`    | Id                             |     | approval: the tool call being approved (its diff/command is already in the log).                                     |
| `options`         | `{option_id, name, kind}[]`    |     | approval: ACP `PermissionOption`s; `kind` ∈ allow_once, allow_always, reject_once, reject_always.                    |
| `response_schema` | JSON Schema                    |     | form.                                                                                                                |
| `url`             | URI                            |     | auth.                                                                                                                |

**`InterruptResolved`** (scope: task). Emitted by the hub when the **first** answer is
accepted (any later answer gets HTTP 409 naming `resolved_by`), or by the runner/hub when
the interrupt is cancelled or expires.

| Field          | Type                                                                                                            | Req | Notes                                                                                                                                           |
| -------------- | --------------------------------------------------------------------------------------------------------------- | --- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `interrupt_id` | Id                                                                                                              | ✓   |                                                                                                                                                 |
| `outcome`      | `{type:"selected", option_id}` \| `{type:"submitted", data}` \| `{type:"cancelled" \| "expired" \| "declined"}` | ✓   | Maps back to ACP `RequestPermissionOutcome` (`selected` / `cancelled`) or the elicitation response (`accept` = submitted, `decline`, `cancel`). |
| `resolved_by`  | Actor                                                                                                           | ✓   | Who answered (user + device), or `{kind:"system"}` for expiry, `{kind:"policy"}` for presets.                                                   |

### 4.6 State

`StateSnapshot{scope, snapshot}` and `StateDelta{scope, patch}` (RFC 6902 against the last
snapshot of the same scope). Scope: thread (both ids null) for every `scope` value.

| `scope`   | ACP source                                                                                      | Snapshot shape                                                                                                                         |
| --------- | ----------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `session` | `available_commands_update`, `current_mode_update`, `config_option_update`, `session/new` modes | `{available_commands: [{name, description, input_hint?}], current_mode_id, modes: [{id, name, description?}], config_options: [...] }` |
| `usage`   | `usage_update` (`used`, `size`, `cost`)                                                         | `{context_used, context_size, cost: {amount, currency} \| null, turn_cost?: {amount, currency}}`                                       |
| `thread`  | — (hub: title, pending-task queue)                                                              | `{title, pending_tasks: [{task_id, preview, author}], machine_id?}`                                                                    |
| `flow`    | — (flow engine)                                                                                 | `{graph, iterations, budget, ledger?}` (defined by M5C-010)                                                                            |

A producer must emit a `StateSnapshot` for a scope before the first `StateDelta` of that
scope in a thread. The hub re-synthesises snapshots at subscription time (§6).

### 4.7 Subagents and steps

| Variant            | Source                                                                   | Fields                                                                                                                                        | Scope |
| ------------------ | ------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- | ----- |
| `SubagentStarted`  | fleet-MCP `delegate`, flow agent steps, harness subagent tool calls      | `subagent_run_id` ✓, `name` ✓, `description`, `child_thread_id`, `child_task_id`, `agent_id`, `parent_tool_call_id`, `parent_subagent_run_id` | run   |
| `SubagentFinished` | child task completed                                                     | `subagent_run_id` ✓, `result`                                                                                                                 | run   |
| `SubagentError`    | child task failed / cancelled                                            | `subagent_run_id` ✓, `message` ✓, `code`                                                                                                      | run   |
| `StepStarted`      | flow step begins                                                         | `step_id` ✓ (dotted path, e.g. `implement.verify`), `step_name` ✓, `iteration`                                                                | run   |
| `StepFinished`     | flow step ends                                                           | `step_id` ✓, `step_name` ✓, `iteration`, `outcome` ✓ (`success` \| `failed` \| `skipped` \| `cancelled`)                                      | run   |
| `Raw`              | a child thread's event nested in its parent; or an unmapped source event | `source` ✓ (`child` \| `acp` \| `a2a` \| `flow`), `child_thread_id`, `event` ✓ (a full child envelope for `child`)                            | run   |

### 4.8 Checkpoints and presence

**`Checkpoint`** (scope: run). End of a turn: shadow-git commit (local) and/or
`DirectoryBackup` (sandbox). Fields: `checkpoint_id` ✓, `kind` ✓ (`shadow_git` \| `backup` \|
`snapshot`), `git_ref` (`refs/sm/checkpoints/<thread>/<n>`), `backup_handle`, `label`, `files_changed`.

**`Presence`** (scope: thread). Published by the Machine DO into every thread hosted on the
machine when its presence changes. Fields: `machine_id` ✓, `state` ✓ (`online` \| `asleep` \|
`offline`), `since` ✓.

### 4.9 Mapper decisions (settled by M1A-006a, SPEC-001a)

These cases are not spelled out by ACP; `AcpTurnMapper` implements them and they are normative:

- **`user_message_chunk`** (ACP replay of the user's own text): inside a run it becomes
  `Raw{source:"acp"}`; outside a task it is dropped. `UserMessage` comes from the hub
  (client-api.md §6); `session/load` replay into `UserMessage` is future work (M1A-006d).
- **Cancel or expiry while a run is paused on an interrupt**: emit
  `InterruptResolved{cancelled|expired}` for each open interrupt, then a short
  `RunStarted{resume}` run that closes open tool calls with `ToolCallEnd{failed}`, then
  `RunError{cancelled}` / `RunError{error, code:"expired"}`. ACP updates queued during the pause
  are dropped.
- **The harness process exits while a task runs** (or a runner restart finds interrupts stored
  by its previous process, SPEC-004): every open interrupt of that task gets
  `InterruptResolved{cancelled}` (nobody can answer it any more; `expired` is only for TTL
  expiry), then `RunError{error, code:"harness_exited"}`. No resume run is emitted. The next
  prompt respawns the harness and resumes the ACP session.
- **A tool call still open at the end of a turn**: closed with `ToolCallEnd{completed}` when the
  stop reason is `end_turn`, `ToolCallEnd{failed}` for any other stop reason or error.
- **`session_info_update`**: only the title maps (`StateDelta{thread}` on `/title`). The hub owns
  `pending_tasks`; a runner never sends a `StateSnapshot{thread}` that would overwrite it.
- **Message boundaries**: a chunk with a different ACP `messageId` (when present) closes the
  open message and starts a new one, as does any update of another kind.
- **`ToolCallResult.content`** carries text and https file parts only; images, audio and binary
  resources stay in `raw`.
- **Size caps**: diff and text deltas are split above 96 KiB, terminal output above 64 KiB per
  event; the closing terminal snapshot keeps the last 128 KiB of output so it fits §2.
- **Invalid ids**: ACP tool-call ids outside the Id pattern are hashed to `tc_<hash>`; permission
  options with an invalid `optionId` are dropped.

## 5. Tasks, runs and ordering rules

Definitions (plan §1.2): a **task** is one prompt turn; a **run** is a contiguous stretch
of agent execution inside a task. A task has one run, plus one more per interrupt round-trip.

Producers must emit, and the Thread DO rejects (HTTP 422 / tunnel `nack`) any append that violates:

1. **Task shape.** `UserMessage` → `RunStarted{prompt}` → … → a terminal event
   (`RunFinished` or `RunError`). A task whose last run ended `RunFinished{interrupt}`
   continues with `InterruptResolved` (one per open interrupt) → `RunStarted{resume,
parent_run_id}` → … . The task ends at the first `RunFinished{success}` or `RunError`.
2. **One open run per task, one running task per thread.** A run is open from its
   `RunStarted` to its terminal event. While a task is running, new prompts become pending
   tasks (`StateDelta{thread}` on `/pending_tasks`); their `UserMessage` is emitted when
   they start.
3. **Run membership.** Every run-scoped event carries the `run_id` of the currently open
   run of its task, and appears after that run's `RunStarted` and before its terminal event.
4. **Start before Content/End.** For each `message_id`: exactly one `*Start`, then
   `*Content`s, then exactly one `*End`; all within one run. Messages are closed before the
   run's terminal event.
5. **Tool calls.** The first event for a `tool_call_id` is `ToolCallStart`; `ToolCallEnd` is
   last. A tool call may stay open across an interrupt (approval of that very call) but is
   closed before the task's final terminal event; on `RunError` the producer first closes
   every open tool call with `ToolCallEnd{failed}`.
6. **Interrupts.** An `Interrupt` is followed, in the same run, by
   `RunFinished{interrupt, interrupt_ids ⊇ every open interrupt}`. While a run is paused the
   runner buffers further ACP updates for that task and emits them after the resume
   `RunStarted`. `InterruptResolved` for every listed id precedes the resume `RunStarted`.
   Cancelling the task resolves open interrupts as `cancelled` (ACP permission answered
   `cancelled`) and ends with `RunError{cancelled}`. An expired approval ends the task with
   `InterruptResolved{expired}` + `RunError{error, code: "expired"}` (the UI offers "retry turn").
7. **Subagents and steps closed.** Every `SubagentStarted` is closed by `SubagentFinished` or
   `SubagentError`, and every `StepStarted` by a `StepFinished` with the same `step_id` and
   `iteration`, before the run's `RunFinished{success}` or `RunError`. (A run that ends
   `RunFinished{interrupt}` may leave them open; the resume run closes them.)
8. **Snapshot before delta**, per state scope and per `activity_id`.
9. **Thread-scoped events** (`Presence`, `StateSnapshot`, `StateDelta`) may appear anywhere,
   including inside an open run.

Golden tests in `crates/protocol` check these rules with a pure `validate_sequence()`
over a list of envelopes (M1A-006a); the Thread DO runs the same function on append.

## 6. Subscription and `?after=seq` catch-up

`GET /threads/{thread_id}/subscribe?after=<seq>` upgrades to a WebSocket (hibernatable, on
the Thread DO). Messages from the hub are JSON objects:

| Message                                         | Meaning                                                                                                                                    |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `{"type":"hello","head_seq":N,"replay_from":R}` | Sent first. `N` = the last stored seq; `R` = the seq of the first replayed event.                                                          |
| `{"type":"snapshot","at_seq":K,"states":{...}}` | Current `StateSnapshot` content per scope (`session`, `usage`, `thread`, `flow` when present), as of seq `K`. Not a log event; has no seq. |
| `{"type":"event","envelope":{...}}`             | One stored envelope.                                                                                                                       |
| `{"type":"live"}`                               | Catch-up done; everything after this is live.                                                                                              |

Catch-up algorithm (Thread DO):

1. Let `after` be the query value (default 0). The **run boundary** for `after` is the seq
   of the `RunStarted` of the run that was open at `after` (if a run was open), else `after + 1`.
   Replay starts there: `R = min(boundary, after + 1)`. This lets a client that reconnects in
   the middle of a run rebuild open messages, tool calls and activities from their start.
2. Send `hello`, then one `snapshot` (as of `R - 1`), then every stored event with
   `seq ≥ R` in order, then `live`, then live events.
3. Clients deduplicate by `seq`: events with `seq ≤` their last applied seq only rebuild
   in-progress state. A client never re-POSTs to catch up.

Clients **send** nothing on the subscription except `{"type":"ping"}` (answered with
`{"type":"pong"}`); commands (prompts, approvals, cancel) go through the HTTP API with
idempotency keys.

## 7. Edge adapters (only place A2A / AG-UI exist)

| Direction                        | Where                             | Rule                                                                                                                                                                                                                                                                                                                                                                                                        |
| -------------------------------- | --------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ACP → TurnEvent                  | `protocol::mapping` (runner)      | Primary mapping; every row of §4 has a golden fixture (ACP JSON in → envelopes out).                                                                                                                                                                                                                                                                                                                        |
| TurnEvent → A2A, A2A → TurnEvent | `protocol::mapping` + `a2a-edge`  | Plan §1.1 "A2A adapter" column; task ↔ task, thread ↔ contextId, Interrupt ↔ INPUT_REQUIRED/AUTH_REQUIRED.                                                                                                                                                                                                                                                                                                  |
| TurnEvent → AG-UI                | `protocol::mapping` + `agui-edge` | Rename fields to camelCase; `Activity` → `ActivitySnapshot`/`ActivityDelta` (`activityType` upper-case); `Reasoning*` wrapped in `ReasoningStart`/`ReasoningEnd`; `Interrupt` + `RunFinished{interrupt}` → `RunFinished{outcome:{type:"interrupt"}}`; `Presence` → `Custom`; `UserMessage` → `TextMessageStart/Content/End` with role `user`; `InterruptResolved` → dropped (AG-UI resumes with a new run). |

No other crate depends on A2A or AG-UI types.

## 8. Schemas and examples

```
crates/protocol/schemas/turn-event/
  envelope.schema.json        # the envelope; event = oneOf every events/*.schema.json
  common.schema.json          # Id, Timestamp, Actor, ContentPart, JsonPatch, Location, Chunk
  events/<Type>.schema.json   # one per variant (27)
  examples/<Type>.json        # one valid envelope per variant (+ extra: Activity.plan/terminal/policy,
                              #   RunFinished.interrupt, RunStarted.resume)
  validate.py                 # validates every example against the envelope schema
```

JSON Schema draft 2020-12. `python3 crates/protocol/schemas/turn-event/validate.py` (needs
the `jsonschema` package) must pass after any change. Rust tests (MVP-000, M1A-006a) round-trip
every example through `protocol::turn_event` serde types; JSON from Rust must validate
against these schemas (`jsonschema` crate in dev-dependencies).
