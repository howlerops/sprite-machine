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

- `status`: `idle` (no task started, or the last run ended in an error or cancel), `running`,
  `needs_you` (an approval is open), `done` (the last task ended in `RunFinished{success}` and
  no task is running or queued). `done` lasts until the next prompt, which makes the thread
  `running` again.
- `updated_at` and `last_seq` are the time and the event sequence of the **last index-changing
  event**, not of the last event: a new prompt, a run start or end, an approval opening or
  being answered, a title change, archive or delete. Streamed output and session-state changes
  (`StateDelta{session}`, usage) do not move them, so the list never costs a write per token.
  A client that needs the live head uses `head_seq` from `GET /v1/threads/{thread_id}` or the
  subscription's `hello`. The list is ordered by `updated_at`, newest first.
- A thread hosted before the index existed, or whose index write failed, appears after the
  hub's hourly reconcile (or at its next index-changing event).
- A deleted thread (`DELETE /v1/threads/{thread_id}`) is `404 not_found` on every
  `/v1/threads/{thread_id}/...` route, the same answer as an unknown thread, and is not listed.

**`GET /v1/threads/{thread_id}`** → 200 `{ "thread_id", "title", "machine_id", "agent", "workspace", "head_seq", "states": { "session": {…}, "usage": {…}, "thread": {…} } }`
(same `states` as the subscription `snapshot` message).

**`GET /v1/threads/{thread_id}/subscribe?after=<seq>`**: WebSocket, turn-event.md §6.
Native clients send `Authorization: Bearer`. Browsers cannot set headers on a WebSocket, so
(SPEC-006) they first call **`POST /v1/threads/{thread_id}/subscribe-ticket`** (Bearer, viewer
role, same thread/org check as subscribe) → 201 `{ "ticket", "expires_at" }`: 256 random bits,
only its SHA-256 stored, **single use, valid 60 seconds**, bound to the session, the user and
this thread. The page then opens `…/subscribe?after=<seq>&ticket=<ticket>`; the hub consumes
the ticket on the upgrade. A missing, reused, expired or other-thread ticket → 401 (same answer).
The session token never appears in a URL, and the ticket never appears in logs or audit
details. A ticket whose session was revoked before use is refused.

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

### 5.8 Push notifications

> M2-011. Web Push (RFC 8291 aes128gcm, RFC 8292 VAPID); APNs/FCM later behind the same hub
> provider trait.

- **`GET /v1/push/vapid-public-key`** → 200 `{ "public_key" }` (base64url, uncompressed P-256 point) (the page passes it to `pushManager.subscribe`).
- **`POST /v1/push/subscriptions`** `{ "endpoint", "keys": { "p256dh", "auth" } }` (member) → 201,
  or 200 with the same id when this user and device already registered the endpoint.
  **`DELETE /v1/push/subscriptions`** `{ "endpoint" }` → 204. Both are idempotent by the endpoint
  itself, so they are the one exception to §3: `Idempotency-Key` is accepted but not required.
- **`GET` / `PUT /v1/push/prefs`** `{ "classes": { "approval_needed", "task_done", "task_failed", "machine_back" },
  "muted_threads": [..], "quiet_hours": { "start": "22:00", "end": "07:00", "tz": "Europe/Berlin" } }`.
- **Payload** (encrypted to the subscription): `{ "kind", "thread_id", "task_id", "interrupt_id"?,
  "title", "actions"? }`. `title` is generic ("Approval needed", "Task finished", "Task failed");
  no prompt text, tool input, file names or agent output ever appears in a push. A `machine_back`
  also carries `"machine_id"` (its title stays generic: "Machine back online"). `actions` (for an
  approval) carries `{ "action", "option_id" }` pairs; the service worker answers through
  §5.4 with the page's session — there is no unauthenticated action route.
- One pending notification per task (`Topic` = task id). A user cancel (`RunError{cancelled}`)
  sends nothing.

### 5.9 GitHub sign-in, members and invites

> M2-003b. OAuth authorization code + PKCE (RFC 7636, S256 only) for the desktop app (RFC 8252) and
> the web page. The hub is the authorization server for the client and the confidential client of
> GitHub (it holds `GITHUB_CLIENT_SECRET`; the client never sees it). The result is an ordinary
> session token (§2) with the user's role in the org.

**Sign-in** (the client opens the first URL in the system browser):

1. **`GET /v1/auth/github/authorize`** `?redirect_uri&code_challenge&code_challenge_method=S256&state[&org_id]`
   → 302 to GitHub. `redirect_uri` must be exactly one of: `http://127.0.0.1:<port>/callback` or
   `http://[::1]:<port>/callback` (any port; the desktop app listens there, RFC 8252 §7.3), or the
   hub's own `<origin>/auth/github/callback` (the web page). Anything else (`localhost`, `https`,
   another path, a query, another host) is **400 `invalid_request`** with no redirect. `code_challenge`
   is the 43-character S256 challenge; `plain` is refused. `state` (8-128 of `[A-Za-z0-9-._~]`) is
   required and returned unchanged. `org_id` picks the org of the session; default is the
   user's oldest membership.
2. GitHub redirects to **`GET /auth/github/callback?code&state`** (register this URL in the GitHub OAuth
   app). The hub's `state` is single use, 10 minutes, bound to the hub's own PKCE verifier for the
   GitHub leg. A missing, used or expired state is **400**, never a redirect. The hub exchanges the
   GitHub code with the client secret and that verifier, reads the GitHub user, and maps it to a hub
   user **by GitHub user id** (not login). An account with no hub user and no open invite, or with no
   membership in the requested org, gets `…?error=access_denied&state=<state>` and nothing is created.
3. The hub redirects to `redirect_uri?code=<hub code>&state=<state>` (the web page gets
   `<origin>/#/auth/github?code=…&state=…`, in the fragment). The code is single use, 2 minutes.
4. **`POST /v1/auth/github/token`** `{ "code", "code_verifier", "redirect_uri" }` (no auth) → 201
   `{ "token", "expires_at", "user", "org", "role" }` (like §5.7). Any mismatch (unknown, used or expired
   code, wrong verifier, other `redirect_uri`) is **400 `invalid_grant`** and burns the code.
   Audited (`auth.github.signin`; `member.invite.accept` for each invite taken).

**Members and invites** (all scoped to the org of the session; no session is 401):

| Route | Role | Notes |
| ----- | ---- | ----- |
| `GET /v1/members` | viewer | `{ members: [{ user_id, display_name, github_login, role, joined_at }] }` |
| `POST /v1/invites` `{ github_login, role? }` | admin | 201 `{ invite: { id, github_login, role, expires_at_ms } }`; `role` (default `member`) may not exceed the inviter's own (403); 409 if the account is already a member or has an open invite; the login is stored lowercase; valid 14 days |
| `GET /v1/invites` | admin | open invites |
| `DELETE /v1/invites/{id}` | admin | 404 if not an open invite of this org |
| `PATCH /v1/members/{user_id}` `{ role }` | admin | never above your own role, never yourself, 404 for another org's user |
| `DELETE /v1/members/{user_id}` | admin | same limits; the member's sessions in the org end at once |

**Hardening** (M2-003e):

- `authorize` is unauthenticated and writes a row, so it is limited per `CF-Connecting-IP`: 10 starts
  per minute, then **429 `rate_limited`** with `Retry-After`. No such header (local dev) means no limit.
- The hourly cron deletes expired `oauth_requests`, `oauth_codes` and `org_invites` rows.
- A GitHub login can be renamed and re-registered, so a login alone proves little. A user an admin
  created with a login and no GitHub id is **linked by login only if the user is younger than 7 days**;
  the link is audited (`auth.github.link`). An older one is never linked by login: that sign-in is
  refused like an unknown account (`access_denied`) unless it has an open invite, in which case a new
  user is created and the stale user loses the login. Once linked, the account is known by its GitHub id.

An invite is accepted by the first GitHub sign-in of that account, which makes it a member with the
invite's role. Every mutation is audited (`member.invite`, `member.invite.revoke`, `member.role`,
`member.remove`).

Hub configuration: var `GITHUB_CLIENT_ID`, Worker secret `GITHUB_CLIENT_SECRET`, optional var
`HUB_PUBLIC_URL` (default: the request's origin). `GITHUB_API_BASE` (dev and tests only) points
authorize, token and user at a mock GitHub; unset means github.com and api.github.com. Unset client
id: **503 `not_configured`**.

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
