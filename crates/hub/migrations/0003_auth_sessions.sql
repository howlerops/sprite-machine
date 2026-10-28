-- M2-003a: session tokens, the one-time owner bootstrap, and the request id on audit rows.
--
-- Only the SHA-256 (hex) of a session token is stored; the token itself (`smt_` + 256 random
-- bits) is shown once, when it is issued. A session belongs to one user, one device and one
-- org. Revoking sets `revoked_at` (soft, so the audit trail still names it); `expires_at` is
-- an ISO-8601 UTC text like every timestamp here, so it compares as text.

CREATE TABLE sessions (
  id           TEXT    PRIMARY KEY NOT NULL,
  token_hash   TEXT    NOT NULL UNIQUE CHECK (length(token_hash) = 64),
  user_id      TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  device_id    TEXT    NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  org_id       TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  created_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  expires_at   TEXT    NOT NULL,
  revoked_at   TEXT,
  last_used_at TEXT
);
CREATE INDEX sessions_by_user ON sessions(user_id);
CREATE INDEX sessions_by_org ON sessions(org_id);

-- One row, ever: the claim of the first-owner bootstrap. `id` is pinned to 1, so a second
-- INSERT fails on the primary key; that is what makes a concurrent second attempt a 409.
CREATE TABLE auth_bootstrap (
  id         INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
  org_id     TEXT,
  user_id    TEXT,
  claimed_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- The request id (x-request-id) of the call that caused the mutation. Adding a column does
-- not touch the append-only triggers.
ALTER TABLE audit_log ADD COLUMN request_id TEXT NOT NULL DEFAULT '';
