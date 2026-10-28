# Spec: client ↔ hub HTTP API (v1)

> Status: **normative** · SPEC-003 · written by the ledger leader after the tracer bullet (MVP-000).
> Companions: `docs/specs/turn-event.md` (events and the subscription), `docs/specs/tunnel.md`
> (runner side). Implemented by M2-008 (threads), M2B-002 (machines), M2-003a (auth), M3-001 and
> the mobile web page (clients). Workers implement this spec; they never change it.

## 1. Principles

- Clients (desktop app, mobile web page, TUI, native mobile) **read** through the thread
  subscription WebSocket (turn-event.md §6) and **write** through the HTTP routes below. A client
  never re-POSTs to catch up and never sends commands over the subscription.
- Every write is **idempotent**: it carries an `Idempotency-Key` header (§3), so a client's
  outbound queue (plan §4.4) can retry blindly after a timeout, a reload or going offline.
- The hub turns writes into TurnEvents (appended to the Thread DO) and tunnel `command` frames
  (queued on the Machine DO, delivered when the runner is online). Effects are visible on the
  subscription; HTTP responses only acknowledge.
- JSON bodies, `snake_case` fields, ids per turn-event.md §2.

## 2. Auth

- Every route requires `Authorization: Bearer <access token>` (issued by device-code pairing or
  GitHub OAuth, M2-003a / M2B-005). The token identifies the **user and device**; the hub derives
  `author` / `resolved_by` (`Actor`) from it. Clients never send an `Actor`.
- Until M2-003a lands: the hub accepts only the literal token `dev` and only when the dev var
  `DEV_FIXTURES=1` is set; the actor is then `{kind:"user", id:"usr_dev", device_id:"dev_dev"}`.
  Any request without a valid token gets 401.
- A thread is visible to members of the org that owns its machine; roles (plan §4.2) gate writes
  (`viewer` gets 403 on every POST/DELETE).

## 3. Idempotency

- `Idempotency-Key: <1–128 chars, [A-Za-z0-9_:.-]>` is **required** on every POST and DELETE
  (missing → 400 `idempotency_key_required`).
- The hub stores `(key, user) → (request hash, status, response body)` for **24 hours**:
  - same key, same body → the stored response is replayed (same status), no new effect;
  - same key, different body → 422 `idempotency_mismatch`.
- The same key travels down the tunnel as the command's `idempotency_key` (tunnel.md §3.5).
- Clients generate the key once per user intent (ULID, prefix `idk_`) and persist it with the
  queued intent.

## 4. Errors

Every non-2xx response has this body:

```json
{
  "error": "already_resolved",
  "message": "this approval was already answered",
  "resolved_by": { "kind": "user", "id": "usr_2", "name": "Sam", "device_id": "dev_laptop" }
}
```

| Status | `error`                                                                       | When                                                   |
| ------ | ----------------------------------------------------------------------------- | ------------------------------------------------------ |
| 400    | `invalid_request`, `idempotency_key_required`                                 | malformed body / header                                |
| 401    | `unauthorized`                                                                | missing or invalid token                               |
| 403    | `forbidden`                                                                   | role or org does not allow it                          |
| 404    | `not_found`                                                                   | unknown thread, task, interrupt, machine               |
| 409    | `already_resolved` (+ `resolved_by`), `task_started`, `thread_busy_elsewhere` | first answer already won; pending task already started |
| 410    | `expired`                                                                     | the interrupt's `expires_at` passed                    |
| 422    | `idempotency_mismatch`, `invalid_sequence`                                    | key reused with another body                           |
| 429    | `rate_limited`                                                                | with `Retry-After`                                     |

## 5. Routes

All paths are under `https://<hub>/v1`.

### 5.1 Machines

**`GET /v1/machines`** → 200

```json
{
  "machines": [
    {
      "machine_id": "mch_laptop",
      "name": "my-laptop",
      "kind": "local",
      "presence": { "state": "online", "since": "2026-10-27T22:00:00.000Z" },
      "capabilities": {
        "harnesses": ["claude", "opencode"],
        "spawn": ["same_dir", "worktree"],
        "max_sessions": 32
      }
    }
  ]
}
```

### 5.2 Threads

**`POST /v1/threads`**: create a thread on a machine and (normally) start its first task. The
hub creates the Thread DO, records the machine as the thread's host, and queues a tunnel
`dispatch` command (tunnel.md §3.5).

```json
{
  "machine_id": "mch_laptop",
  "agent": "claude",
  "workspace": { "path": "/Users/me/src/app", "spawn": "worktree" },
  "permission_mode": "default",
  "title": "Upgrade axum",
  "prompt": {
    "task_id": "tk_01JBX0",
    "message_id": "msg_01JBX1",
    "parts": [{ "type": "text", "text": "Upgrade axum to 0.8" }]
  }
}
```

→ 201 `{ "thread_id": "th_01JBX2", "task_id": "tk_01JBX0", "status": "started" }`.
`prompt` may be omitted (thread created idle). `task_id`/`message_id` are optional client-chosen
ids (so an offline client can render its own message before the hub answers); the hub generates
them when absent. Threads started on the runner itself (TUI, `/rc`) appear through tunnel
`thread_open` instead; clients learn about them from `GET /v1/threads`.

**`GET /v1/threads?machine_id=&status=&q=&cursor=`** → 200 `{ "threads": [ { "thread_id", "title", "machine_id", "agent", "status": "idle|running|needs_you|done", "updated_at", "last_seq" } ], "next_cursor": null }`
(the D1 task index; `q` is FTS search, M2-\*).

**`GET /v1/threads/{thread_id}`** → 200 `{ "thread_id", "title", "machine_id", "agent", "workspace", "head_seq", "states": { "session": {…}, "usage": {…}, "thread": {…} } }`
(same `states` as the subscription `snapshot` message).

**`GET /v1/threads/{thread_id}/subscribe?after=<seq>`**: WebSocket, turn-event.md §6.

### 5.3 Prompts and pending tasks

**`POST /v1/threads/{thread_id}/prompts`**

```json
{
  "task_id": "tk_01JBX5",
  "message_id": "msg_01JBX6",
  "parts": [{ "type": "text", "text": "Now add a test" }]
}
```

- Thread idle → 202 `{ "task_id": "tk_01JBX5", "status": "started" }`. The hub appends
  `UserMessage` (§6) and queues a tunnel `prompt` command.
- A task is running → 202 `{ "task_id": "tk_01JBX5", "status": "pending", "position": 1 }`. The
  hub appends `StateDelta{thread}` adding the task to `/pending_tasks`. When the running task
  ends, the hub starts the next pending task (UserMessage + `prompt` command) in order.
- The machine is offline → same as above; commands wait in the Machine DO queue
  (tunnel.md §5) and the thread state shows the machine's presence.

**`GET /v1/threads/{thread_id}/pending`** → 200 `{ "pending": [ { "task_id", "preview", "author", "queued_at" } ] }`

**`DELETE /v1/threads/{thread_id}/pending/{task_id}`** → 204 (removed; `StateDelta{thread}`
emitted), or 409 `task_started` if it already started (then use cancel).

### 5.4 Interrupts

**`POST /v1/threads/{thread_id}/interrupts/{interrupt_id}`**

```json
{ "outcome": { "type": "selected", "option_id": "allow" } }
```

`outcome` is one of `{type:"selected", option_id}` (approval), `{type:"submitted", data}` (form),
`{type:"declined"}` / `{type:"cancelled"}` (form/auth). The **first** accepted answer wins:

- 200 `{ "interrupt_id": "int_1", "resolved_by": {…} }`. The hub appends `InterruptResolved`
  (turn-event.md §4.5) and queues a tunnel `approval` command.
- Any later answer (different idempotency key) → 409 `already_resolved` with `resolved_by`.
- After `expires_at` → 410 `expired`.

### 5.5 Cancel

**`POST /v1/threads/{thread_id}/cancel`** `{ "task_id": "tk_01JBX5" }` → 202. The hub queues a
tunnel `cancel` command; the runner emits `InterruptResolved{cancelled}` for open interrupts and
`RunError{cancelled}` (turn-event.md §5 rule 6). Cancelling a task that already ended → 200 with
`{ "status": "already_finished" }` (no effect). To drop a pending task use §5.3 DELETE.

### 5.5a Session mode and config

**`POST /v1/threads/{thread_id}/session`** `{ "mode_id": "plan", "config": { "id": "model", "value": "opus" } }`
→ 202. Member role. The hub queues a tunnel `session_config` command (tunnel.md §3.5); the result
appears as `StateDelta{session}` on the subscription. Values must come from the thread's current
`session` state (`modes`, `config_options`), else 400 `invalid_request`.

### 5.6 Attachments

**`POST /v1/attachments`** `{ "name", "mime_type", "size" }` → 201 `{ "upload_url", "url", "expires_at" }`:
the client PUTs the bytes to the signed R2 `upload_url` (≤ 20 MB) and uses `url` in a
`{type:"file"}` part. (Implemented with WEB-003 / M1A-014.)

### 5.7 Browser sign-in (mobile web page)

> SPEC-005. The phone page (WEB-001) needs a user session before GitHub OAuth (M2-003b, P3)
> exists. Two ways, both ending in an ordinary session token (§2) stored by the page.

**A. QR from the runner (the usual path).** `sm rc` prints a QR code for a one-time link.

1. The runner calls **`POST /v1/devices/signin-links`** with its machine credential
   (`Authorization: Bearer smm_…`, tunnel.md §2) → 201
   `{ "url": "https://<hub>/pair?code=<link code>", "expires_at" }`. The link code is 256 random
   bits, single use, valid **5 minutes**; the hub stores only its SHA-256. It is bound to the org
   of the machine and to the user who **approved** the machine (M2B-005); a machine created by
   the admin API (no approving user) gets 403 `forbidden`.
2. The page at `/pair?code=…` calls **`POST /v1/auth/links/redeem`** `{ "code" }` (no auth) →
   201 `{ "token", "expires_at", "user", "org" }`: a session for that user in that org, with the
   user's role capped at `member`, `device_name` "web". A used, expired or unknown code → 410
   `expired` (same answer for all three). Audited (`session.link_redeem`).
3. The runner never sees the session; the link code never appears in logs, the audit detail or
   the tunnel.

**B. Device code shown by the page.** For a phone with no runner in sight, the page runs the
same RFC 8628 flow as `sm login` (§ M2B-005), for a user session instead of a machine:
**`POST /v1/auth/device/code`** → `{ device_code, user_code, verification_uri, interval,
expires_in }`; a signed-in member approves the user code (desktop app, another browser, or
`POST /v1/auth/device/approve` `{ user_code }`); the page polls **`POST /v1/auth/device/token`**
`{ device_code }` → the session token once. The same rate limits as device pairing apply
(M2B-005a), and the token response names the approving user and org, which the page shows
before it stores the token.

## 6. Who emits what

| Event                                             | Emitted by | When                                                                                                                  |
| ------------------------------------------------- | ---------- | --------------------------------------------------------------------------------------------------------------------- |
| `UserMessage`                                     | **hub**    | when a task **starts** (idle thread, or dequeued from pending), just before the `prompt`/`dispatch` command is queued |
| `StateDelta{thread}` (pending tasks)              | hub        | prompt queued, pending task removed or started                                                                        |
| `InterruptResolved` (selected/submitted/declined) | hub        | when it accepts the first answer                                                                                      |
| `InterruptResolved` (cancelled/expired)           | runner     | on cancel, or on TTL expiry (turn-event.md §5 rule 6)                                                                 |
| `Presence`                                        | hub        | Machine DO presence change                                                                                            |
| everything else                                   | runner     | mapped from ACP (turn-event.md §4)                                                                                    |

The runner therefore does **not** emit `UserMessage` for a `prompt` or `dispatch` command; it
starts with `RunStarted{prompt}`. For a thread opened locally (TUI, `/rc`), the runner emits
the `UserMessage` of prompts typed locally, since the hub never saw them.
