-- M2B-002: machines (plan §4.2 "Machine DO" row, tunnel.md §2, §3.7).
--
-- A machine is one runner identity: a local `sm` or a Sandbox. The admin API
-- (`POST /v1/machines`) mints a credential (`smm_` + 256 random bits) and stores only its
-- SHA-256 (hex); the credential is shown once. `DELETE /v1/machines/{id}` revokes (soft:
-- `revoked_at`, so the audit trail still names the row) and the hash stops matching.
--
-- `presence` / `presence_since` / `last_seen_at` mirror what the Machine DO keeps (the DO is
-- the source of truth, D1 is the org-wide read model for lists); the DO writes them best
-- effort when the presence changes (tunnel.md §3.7: connected -> online; socket lost after an
-- asleep hint -> asleep; lost without one or 3 x heartbeat silence -> offline).
--
-- `agents.kind` (local | managed | byo) already exists since 0002_org_metadata.sql, with its
-- CHECK constraint, so this migration does not touch `agents`.
--
-- Conventions as in 0002: TEXT ids, ISO-8601 UTC text timestamps, enumerations as TEXT + CHECK.

CREATE TABLE machines (
  id              TEXT    PRIMARY KEY NOT NULL,
  org_id          TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  name            TEXT    NOT NULL,
  -- host mode of the runner (tunnel.md §3.1 `capabilities.host_mode`).
  kind            TEXT    NOT NULL CHECK (kind IN ('local', 'sandbox')),
  credential_hash TEXT    NOT NULL CHECK (length(credential_hash) = 64),
  created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  revoked_at      TEXT,
  last_seen_at    TEXT,
  presence        TEXT    NOT NULL DEFAULT 'offline' CHECK (presence IN ('online', 'asleep', 'offline')),
  presence_since  TEXT
);
CREATE INDEX machines_by_org ON machines(org_id, created_at);
