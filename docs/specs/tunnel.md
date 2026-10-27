# Spec: the runner ↔ hub tunnel (v1)

> Status: **normative** · SPEC-002 · written by the ledger leader before fan-out.
> Plan: `docs/PLAN-v2-ade.md` §1 (Runner/sidecar → hub row), §4.1, §4.10 (Connection, Sleep and disconnects).
> Payload format: `docs/specs/turn-event.md`. Types: `crates/protocol/src/tunnel/` (M2B-001 implements this spec;
> MVP-000 implements the subset the tracer needs). Workers implement this spec; they never change it.

## 1. Shape

- Every runner (`sm` on a laptop in `local` mode, or inside a Cloudflare Sandbox in `sandbox`
  mode) keeps **one outbound WebSocket** to its **Machine Durable Object** on the hub. The
  runner never listens on a port.
- **Up** (runner → hub): TurnEvent envelopes, thread registrations, command results.
  **Down** (hub → runner): commands (prompt, approval, dispatch, cancel).
- Each direction is an ordered stream with its own **frame `seq`** and cumulative **`ack`**,
  which gives exactly-once delivery across reconnects (§4). Envelope `seq` (per thread,
  assigned by the Thread DO) is a different number and never appears in tunnel bookkeeping.

```
runner                                   Machine DO (hub)                       Thread DO
  │── WSS GET /v1/machines/{id}/tunnel ──▶│  (Authorization: Bearer <machine credential>)
  │── hello ─────────────────────────────▶│
  │◀──────────────────────────── welcome ─│
  │── event(seq=7) ──────────────────────▶│── append(envelope) ──────────────────▶│ assigns envelope seq
  │◀──────────────────────────── ack(7) ──│◀─────────────────────────── ok ───────│
  │◀──────────────── command(seq=3) ──────│  (queued while the runner was offline)
  │── ack(3) ────────────────────────────▶│
```

## 2. Connection and auth

- URL: `wss://<hub>/v1/machines/{machine_id}/tunnel`. Subprotocol `sm.tunnel.v1`
  (`Sec-WebSocket-Protocol`). The hub refuses any other subprotocol with HTTP 426.
- Auth: `Authorization: Bearer <machine credential>` on the upgrade request. **Never** in the
  URL or in a frame. Local runners get the credential from pairing (M2B-005) and keep it in the
  OS keychain; sandbox runners get a short-lived one in env at start (rotated per start) and
  keep it in memory only (plan §4.1, §4.8).
- The Machine DO accepts at most **one** tunnel per machine. A new authenticated connection
  supersedes the old one: the hub sends `close{code: "superseded"}` on the old socket first.
- The hub uses the **hibernatable WebSocket API**. Heartbeats use the literal strings in §3.7
  so the DO can answer them with `setWebSocketAutoResponse` without waking.
- Every frame is one UTF-8 JSON text message, at most **1 MiB**. Binary messages are a
  protocol error.

## 3. Frames

Every frame is an object with a `t` (type) field. Field names are `snake_case`.
Sequenced frames (`event`, `thread_open`, `command_result` up; `command` down) carry
`seq`; the rest do not.

| Frame            | Direction | Sequenced | Purpose                                                     |
| ---------------- | --------- | --------- | ----------------------------------------------------------- |
| `hello`          | up        | no        | First frame after connect: identity, resume token, cursors. |
| `welcome`        | down      | no        | Hub's answer to `hello`: new resume token, cursors.         |
| `event`          | up        | yes       | One TurnEvent envelope.                                     |
| `thread_open`    | up        | yes       | Registers a thread started on the runner (TUI, `/rc`).      |
| `command_result` | up        | yes       | Outcome of a command that the runner could not apply.       |
| `command`        | down      | yes       | prompt, approval, dispatch, cancel.                         |
| `ack`            | both      | no        | Cumulative ack of the peer's stream.                        |
| `nack`           | down      | no        | The hub rejected one up frame (it still counts as acked).   |
| `presence`       | up        | no        | Runner-reported presence hint (going to sleep, busy).       |
| `ping` / `pong`  | both      | no        | Heartbeat.                                                  |
| `close`          | both      | no        | Reason for closing, sent just before the WebSocket close.   |

### 3.1 `hello` (up, first frame)

```json
{
  "t": "hello",
  "proto": 1,
  "machine_id": "mch_01JBW3K8Q2",
  "runner_version": "0.3.0",
  "resume_token": "rt_01JBW4ZK1Y.9f2c",
  "last_down_seq": 41,
  "capabilities": {
    "host_mode": "local",
    "harnesses": ["claude", "opencode"],
    "max_sessions": 32,
    "spawn": ["same_dir", "worktree"],
    "features": ["checkpoints.shadow_git", "terminals", "attachments"]
  }
}
```

- `resume_token`: the token from the last `welcome`, or `null` on first connect / after the
  runner lost its state.
- `last_down_seq`: the highest command `seq` the runner has **durably processed** (0 if none).
- The runner sends nothing else until it receives `welcome`. If no `welcome` arrives within
  10 s it closes and reconnects with backoff.

### 3.2 `welcome` (down)

```json
{
  "t": "welcome",
  "proto": 1,
  "resumed": true,
  "resume_token": "rt_01JBW51M3C.a7d0",
  "last_up_seq": 118,
  "heartbeat_ms": 20000,
  "max_inflight": 256
}
```

- `resumed: true`: the token matched; both streams continue (§4.3).
  `resumed: false`: a fresh stream (§4.4).
- `last_up_seq`: the highest up `seq` the hub has durably processed on this stream.
- The hub then resends every queued command with `seq > last_down_seq`, in order.
  The runner resends every outbox frame with `seq > last_up_seq`, in order.

### 3.3 `event` (up)

```json
{
  "t": "event",
  "seq": 119,
  "event_id": "evt_01JBW52A0F",
  "envelope": {
    "v": 1,
    "seq": 0,
    "thread_id": "th_1",
    "task_id": "tk_1",
    "run_id": "run_1",
    "ts": "2026-10-27T22:00:03.000Z",
    "event": {
      "type": "TextMessageContent",
      "message_id": "msg_a1",
      "delta": "Adding the route now."
    }
  }
}
```

- `event_id`: a ULID the runner assigns once, when it writes the frame to its outbox; it never
  changes on retransmit. It is the dedupe key on fresh streams (§4.4).
- `envelope.seq` is `0`; the Thread DO assigns the real one.

### 3.4 `thread_open` (up)

A local session started on the runner (TUI, `/rc`, desktop app, auto-connect) registers its
thread so phones see it. Threads created by a `dispatch` command need no `thread_open`.

```json
{
  "t": "thread_open",
  "seq": 120,
  "event_id": "evt_01JBW52B7H",
  "thread_id": "th_01JBW52B6Z",
  "title": "fix flaky test",
  "agent": "claude",
  "workspace": { "path": "/Users/me/src/app", "branch": "main" }
}
```

### 3.5 `command` (down)

```json
{
  "t": "command",
  "seq": 42,
  "idempotency_key": "idk_01JBW5A1QX",
  "issued_at": "2026-10-27T22:01:00.000Z",
  "command": { "kind": "prompt", "...": "per kind, below" }
}
```

`idempotency_key` is the key the **client** sent with its HTTP request (prompt, approval,
cancel, dispatch), so a client retry, a hub retry and a tunnel retransmit all collapse to
one effect. The runner records processed keys in its store for **7 days** and ignores a
command whose key it has already processed (it still acks the frame).

**`prompt`**: start a task in an existing thread. The hub owns the pending-task queue and
sends the next `prompt` only after the thread's running task has ended. The hub has already
appended the task's `UserMessage` (client-api.md §6), so the runner starts with `RunStarted{prompt}`
and does not emit `UserMessage` for `prompt` or `dispatch` commands.

```json
{
  "kind": "prompt",
  "thread_id": "th_1",
  "task_id": "tk_2",
  "message_id": "msg_u2",
  "parts": [
    { "type": "text", "text": "Now add a test" },
    {
      "type": "file",
      "name": "log.txt",
      "mime_type": "text/plain",
      "url": "https://hub.example/att/a2?sig=x",
      "size": 812
    }
  ],
  "author": { "kind": "user", "id": "usr_1", "device_id": "dev_phone" }
}
```

**`approval`**: the accepted (first) answer to an interrupt. The hub has already emitted
`InterruptResolved`; the runner answers ACP (`RequestPermissionOutcome` / elicitation
response) and emits the resume `RunStarted`.

```json
{
  "kind": "approval",
  "thread_id": "th_1",
  "task_id": "tk_1",
  "interrupt_id": "int_1",
  "outcome": { "type": "selected", "option_id": "allow" },
  "resolved_by": { "kind": "user", "id": "usr_1", "device_id": "dev_phone" }
}
```

**`dispatch`**: start a new thread on this machine (`sm rc` server mode, plan §4.10).

```json
{
  "kind": "dispatch",
  "thread_id": "th_01JBW5C0D4",
  "task_id": "tk_01JBW5C0D5",
  "agent": "claude",
  "workspace": { "path": "/Users/me/src/app", "spawn": "worktree" },
  "permission_mode": "default",
  "message_id": "msg_u9",
  "parts": [{ "type": "text", "text": "Upgrade axum to 0.8" }],
  "author": { "kind": "user", "id": "usr_1", "device_id": "dev_phone" }
}
```

**`cancel`**: cancel the running task (ACP `session/cancel`; open permissions answered
`cancelled`). The runner emits `InterruptResolved{cancelled}` for open interrupts and
`RunError{cancelled}`.

```json
{ "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1" }
```

Unknown `kind`: the runner replies `command_result{ok: false, code: "unsupported"}`.

### 3.6 `command_result` (up)

Sent only when a command **cannot** be applied (success shows up as TurnEvents):

```json
{
  "t": "command_result",
  "seq": 121,
  "event_id": "evt_01JBW5D2N1",
  "idempotency_key": "idk_01JBW5A1QX",
  "ok": false,
  "code": "capacity",
  "message": "32 sessions already running"
}
```

Codes: `capacity`, `unknown_thread`, `unknown_interrupt`, `workspace_untrusted`,
`harness_missing`, `unsupported`, `invalid`.

### 3.7 `ack`, `nack`, `presence`, `ping` / `pong`

```json
{ "t": "ack", "seq": 119 }
```

Cumulative: "I have durably processed every frame of your stream up to `seq`". Either side
sends it at least every **50 frames or 200 ms** while frames are unacked, and immediately
after a `welcome` resend completes.

```json
{
  "t": "nack",
  "seq": 119,
  "code": "invalid_sequence",
  "message": "TextMessageContent before TextMessageStart for msg_a1"
}
```

The hub rejected the up frame (`invalid_envelope`, `invalid_sequence` per turn-event.md §5,
`forbidden` = thread not hosted by this machine, `too_large`). A nack **counts as an ack**
for that seq, so one bad frame cannot wedge the stream. The runner logs it, drops the frame
from its outbox, and fails the affected task locally (`RunError{error, code: "protocol"}`
is then emitted as a new frame).

```json
{ "t": "presence", "state": "asleep", "reason": "os_sleep" }
```

Best-effort hint before the OS sleeps (`asleep`) or when the runner is full (`busy`). The
Machine DO's presence is otherwise derived from the socket: connected → `online`; socket
lost after an `asleep` hint → `asleep`; socket lost without one, or no heartbeat for
3 × `heartbeat_ms` → `offline`. Presence changes are published as `Presence` TurnEvents.

Heartbeat frames are **exact literal strings** (so the DO can auto-respond while hibernated):

```
{"t":"ping"}
{"t":"pong"}
```

The runner sends `{"t":"ping"}` every `heartbeat_ms`; the hub answers `{"t":"pong"}`. If the
runner sees no traffic for 3 × `heartbeat_ms`, it closes and reconnects. The runner also
reconnects at once when it detects a wall-clock jump larger than 2 × `heartbeat_ms` (OS
sleep/wake).

### 3.8 `close`

```json
{
  "t": "close",
  "code": "superseded",
  "message": "a newer connection for this machine was accepted"
}
```

Sent just before the WebSocket close frame. The WebSocket close code is 4000 + the number below.

| `code`                | WS   | Sender | Runner behaviour                                                             |
| --------------------- | ---- | ------ | ---------------------------------------------------------------------------- |
| `normal`              | 4000 | both   | Do not reconnect (runner shutting down / machine deleted).                   |
| `superseded`          | 4001 | hub    | Do not reconnect (another instance of this machine connected).               |
| `auth_invalid`        | 4002 | hub    | Do not reconnect; re-pair (`sm login`) or wait for a new sandbox start.      |
| `auth_revoked`        | 4003 | hub    | Do not reconnect; the machine was revoked from the app. Wipe the credential. |
| `version_unsupported` | 4004 | hub    | Do not reconnect; tell the user to update `sm`.                              |
| `protocol_error`      | 4005 | both   | Reconnect with backoff.                                                      |
| `heartbeat_timeout`   | 4006 | both   | Reconnect with backoff.                                                      |
| `server_restart`      | 4007 | hub    | Reconnect with backoff (no penalty: start at the minimum delay).             |
| `going_to_sleep`      | 4008 | runner | (Runner → hub) presence becomes `asleep`.                                    |

Any close without a `close` frame (network loss) is treated like `heartbeat_timeout`.
**Backoff**: exponential from 500 ms, factor 2, cap 30 s, full jitter; reset after a
connection has stayed up for 60 s.

## 4. Exactly-once delivery

### 4.1 Streams

- **Up stream**: frames `event`, `thread_open`, `command_result`, numbered by the runner from
  1, gap-free.
- **Down stream**: `command` frames, numbered by the Machine DO from 1, gap-free.
- A stream lives as long as its **resume-token chain** (§4.3). A fresh stream (§4.4)
  restarts both numberings at 1.

### 4.2 Sender and receiver rules (both directions)

Sender:

1. Write the frame to the **durable outbox** (runner: SQLite; hub: Machine DO SQLite)
   with the next `seq` _before_ sending it.
2. Send frames in `seq` order; keep at most `max_inflight` (default 256) unacked frames on
   the wire.
3. On `ack{seq: n}` (or `nack{seq: n}`), delete outbox frames with `seq ≤ n`.
4. After a reconnect, resend every outbox frame with `seq >` the peer's cursor from
   `hello`/`welcome`, in order.

Receiver:

1. Keep `last` = the highest seq durably processed (persisted with the effect, in the same
   transaction where possible: the Machine DO appends to the Thread DO, then persists `last`).
2. Frame with `seq ≤ last`: duplicate; drop it and re-ack `last`.
3. Frame with `seq == last + 1`: process, persist, `last += 1`, ack (batched per §3.7).
4. Frame with `seq > last + 1`: gap; send `close{protocol_error}`.

Because the hub's Thread DO append and the Machine DO's `last` update are two objects, the
Machine DO also passes `event_id` to the Thread DO, which ignores an `event_id` it has already
appended (kept 24 h). That makes the cross-object step idempotent.

### 4.3 Resume-token lifecycle

1. The hub mints a resume token in every `welcome`: opaque, ≤ 128 chars, bound to
   `machine_id` and the stream, single-use, valid for **7 days**. Only its hash is stored.
2. The runner persists the latest token (local: its state dir, mode 0700; sandbox: its state
   dir, restored with the workspace backup) and presents it in the next `hello`.
3. Valid token → `resumed: true`; the old token is invalidated and the new one issued.
   Cursors from `hello`/`welcome` continue both streams.
4. Missing, unknown, expired or already-used token → `resumed: false` (§4.4). A sandbox
   restored from an old backup lands here, because its token was already used.
5. Revoking a machine invalidates all its tokens and closes the tunnel with `auth_revoked`.

### 4.4 Fresh stream

On `resumed: false` both sides restart numbering at 1:

- The runner renumbers its whole outbox from 1 (same order, same `event_id`s) and sends it.
  The hub dedupes by `event_id` (24 h window), so events delivered on the old stream are not
  appended twice.
- The hub renumbers its queued commands from 1 and sends them. The runner dedupes by
  `idempotency_key` (7-day store), so commands processed on the old stream are not applied twice.

## 5. Offline queue

- Commands for a machine that is not connected are written to the Machine DO's outbox
  (§4.2) and delivered **in order** when the runner reconnects, after `welcome`.
- The hub queues **prompt**, **approval**, **dispatch** and **cancel**. Clients see queued
  commands as "waiting for <machine>" (the thread's pending-task state and the machine's
  presence).
- A queued `prompt` (or `dispatch`) that the user removes before delivery is deleted from the
  outbox and the remaining frames are renumbered before the next send; once delivered it can
  only be cancelled (`cancel`).
- A queued `approval` whose interrupt expired before delivery is still delivered; the runner
  answers ACP `cancelled` for it and emits the expiry events (turn-event.md §5 rule 6).
- Queued commands expire after **7 days**: the hub drops them and emits
  `RunError{error, code: "expired"}` on the affected tasks.
- The runner's **outbox survives restarts** (SQLite). On start it reconnects and resends
  every un-acked frame in `seq` order; a crash between "write to outbox" and "send" loses
  nothing.

## 6. Validation by the hub

For every up frame, the Machine DO checks, in order:

1. The frame parses and matches this spec; else `close{protocol_error}`.
2. `event.envelope.thread_id` (or `thread_open.thread_id`) is a thread hosted by this
   machine (registered by `thread_open` or created by a `dispatch` to it); else `nack{forbidden}`.
3. The envelope validates against `crates/protocol/schemas/turn-event/envelope.schema.json`
   and its size ≤ 256 KiB; else `nack{invalid_envelope}` / `nack{too_large}`.
4. The Thread DO accepts it under turn-event.md §5; else `nack{invalid_sequence}`.

The machine identity comes **only** from the credential that authenticated the socket
(plan §4.8 item 4), never from `hello.machine_id`; a mismatch closes with `auth_invalid`.

## 7. Implementation notes for workers

- Types (M2B-001): one serde enum `Frame` tagged by `t`, plus `Command` tagged by `kind`;
  round-trip tests against every JSON example in this file.
- A pure `Sender` / `Receiver` state machine (no IO) implements §4.2–§4.4 and is tested with a
  simulated reconnect (resumed and fresh), duplicate frames, and a nack.
- MVP-000 needs only: `hello`, `welcome`, `event`, `ack`, `command{prompt, approval}`,
  `{"t":"ping"}`/`{"t":"pong"}`, with in-memory outboxes.
