-- M2-016: org-wide metadata in D1 (plan §4.2): users, orgs, members/roles, devices, agents
-- (kind, card cache), workspaces, the task index with FTS5 search, notification prefs, the
-- audit log and usage rollups.
--
-- Not here: the `machines` table (M2B-002's migration), and the Thread / Machine / Flow
-- Durable Object storage (M2-008, M2B-002, M5C-002).
--
-- Conventions: ids are TEXT (ULIDs or UUIDs minted by the hub). Timestamps are ISO-8601 UTC
-- text. Booleans are INTEGER 0/1 with a CHECK. Enumerations are TEXT with a CHECK, so a
-- migration is the only place a new value is added. Foreign keys are on (D1 enables them,
-- and the test fake turns them on too).

-- ---- identity and org membership ---------------------------------------------------------

CREATE TABLE orgs (
  id                        TEXT    PRIMARY KEY NOT NULL,
  slug                      TEXT    NOT NULL UNIQUE,
  name                      TEXT    NOT NULL,
  -- Per-org retention overrides (plan §4.2 "Retention"): defaults are 90 days for events and
  -- 30 days for terminal streams.
  event_retention_days      INTEGER NOT NULL DEFAULT 90 CHECK (event_retention_days > 0),
  terminal_retention_days   INTEGER NOT NULL DEFAULT 30 CHECK (terminal_retention_days > 0),
  -- Plan §4.2 "Auth": trusted devices can be required per org.
  require_trusted_devices   INTEGER NOT NULL DEFAULT 0 CHECK (require_trusted_devices IN (0, 1)),
  created_at                TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at                TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE users (
  id            TEXT    PRIMARY KEY NOT NULL,
  -- GitHub OAuth (PKCE) is the desktop/web login; device-code pairing creates devices, not users.
  github_login  TEXT    UNIQUE,
  email         TEXT,
  display_name  TEXT    NOT NULL DEFAULT '',
  created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- Roles (plan §4.2 "Auth"): owner, admin, member, viewer.
CREATE TABLE members (
  org_id      TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  user_id     TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role        TEXT    NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'viewer')),
  created_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (org_id, user_id)
);
CREATE INDEX members_by_user ON members(user_id);

-- Runners and phones paired to a user (device-code / QR pairing). `trusted` is set by an
-- admin or by the pairing flow; `revoked_at` is a soft revoke so the audit trail still names it.
CREATE TABLE devices (
  id            TEXT    PRIMARY KEY NOT NULL,
  org_id        TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  user_id       TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name          TEXT    NOT NULL,
  platform      TEXT    NOT NULL DEFAULT '',
  trusted       INTEGER NOT NULL DEFAULT 0 CHECK (trusted IN (0, 1)),
  revoked_at    TEXT,
  last_seen_at  TEXT,
  created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX devices_by_org_user ON devices(org_id, user_id);

-- ---- agents and workspaces ----------------------------------------------------------------

-- kind: `local` (a runner on a device; no URL, reached through its tunnel), `managed` (hosted
-- by us), `byo` (bring-your-own A2A agent at an https URL, SSRF-checked before use).
-- card_json is the cached A2A agent card; card_fetched_at says when it was read.
CREATE TABLE agents (
  id              TEXT    PRIMARY KEY NOT NULL,
  org_id          TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  kind            TEXT    NOT NULL CHECK (kind IN ('local', 'managed', 'byo')),
  name            TEXT    NOT NULL,
  url             TEXT,
  device_id       TEXT    REFERENCES devices(id) ON DELETE SET NULL,
  card_json       TEXT    CHECK (card_json IS NULL OR json_valid(card_json)),
  card_fetched_at TEXT,
  created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  -- Local agents have no URL; BYO agents must have one.
  CHECK ((kind = 'local') = (url IS NULL)),
  UNIQUE (org_id, name)
);
CREATE INDEX agents_by_device ON agents(device_id);

CREATE TABLE workspaces (
  id            TEXT    PRIMARY KEY NOT NULL,
  org_id        TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  name          TEXT    NOT NULL,
  -- `local` (on a device) or `cloud` (a Sandbox workspace, §4.3).
  kind          TEXT    NOT NULL CHECK (kind IN ('local', 'cloud')),
  owner_user_id TEXT    REFERENCES users(id) ON DELETE SET NULL,
  device_id     TEXT    REFERENCES devices(id) ON DELETE SET NULL,
  status        TEXT    NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'stopped', 'archived')),
  created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  archived_at   TEXT,
  UNIQUE (org_id, name)
);
CREATE INDEX workspaces_by_device ON workspaces(device_id);

-- ---- task index ----------------------------------------------------------------------------

-- The index of threads as tasks, for lists and search. The Thread DO owns the events; this row
-- is the org-wide pointer and the searchable title. `seq` is the rowid: it never changes
-- (unlike the rowid of a table without an INTEGER PRIMARY KEY, which VACUUM may renumber), so
-- the FTS5 rows below stay in step. `id` is the public task id.
--
-- title_source: who set the title (the user, the agent's summary, or a derived first line).
-- labels: a JSON array of strings, e.g. '["backend","urgent"]'.
CREATE TABLE tasks (
  seq           INTEGER PRIMARY KEY AUTOINCREMENT,
  id            TEXT    NOT NULL UNIQUE,
  org_id        TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  workspace_id  TEXT    REFERENCES workspaces(id) ON DELETE SET NULL,
  thread_id     TEXT    NOT NULL,
  title         TEXT    NOT NULL DEFAULT '',
  title_source  TEXT    NOT NULL DEFAULT 'derived' CHECK (title_source IN ('user', 'agent', 'derived')),
  status        TEXT    NOT NULL DEFAULT 'open'
                CHECK (status IN ('open', 'running', 'waiting', 'done', 'failed', 'cancelled')),
  labels        TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(labels)),
  created_by    TEXT    REFERENCES users(id) ON DELETE SET NULL,
  created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  archived_at   TEXT,
  UNIQUE (org_id, thread_id)
);
-- The task list: newest first within an org, archived ones filtered by the caller.
CREATE INDEX tasks_list ON tasks(org_id, archived_at, updated_at);
CREATE INDEX tasks_by_status ON tasks(org_id, status, updated_at);
CREATE INDEX tasks_by_workspace ON tasks(workspace_id);

-- Full-text search over task titles. A standalone FTS5 table keyed by tasks.seq (its rowid),
-- kept in step by the triggers below. `org_id` and `task_id` are stored but not indexed, so a
-- search filters by org after the MATCH.
CREATE VIRTUAL TABLE tasks_fts USING fts5(
  title,
  org_id UNINDEXED,
  task_id UNINDEXED,
  tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER tasks_fts_insert AFTER INSERT ON tasks
BEGIN
  INSERT INTO tasks_fts (rowid, title, org_id, task_id)
  VALUES (new.seq, new.title, new.org_id, new.id);
END;

CREATE TRIGGER tasks_fts_update AFTER UPDATE OF title, org_id, id ON tasks
BEGIN
  DELETE FROM tasks_fts WHERE rowid = old.seq;
  INSERT INTO tasks_fts (rowid, title, org_id, task_id)
  VALUES (new.seq, new.title, new.org_id, new.id);
END;

CREATE TRIGGER tasks_fts_delete AFTER DELETE ON tasks
BEGIN
  DELETE FROM tasks_fts WHERE rowid = old.seq;
END;

-- ---- per-user notification preferences -----------------------------------------------------

-- Event class x mute x quiet hours (plan §4.2 "Notifications"). quiet_start_min and
-- quiet_end_min are minutes after midnight in quiet_tz (an IANA zone); both NULL means no
-- quiet hours. Prefs are per org, so one user can mute one org and not another.
CREATE TABLE notification_prefs (
  user_id         TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  org_id          TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  event_class     TEXT    NOT NULL,
  muted           INTEGER NOT NULL DEFAULT 0 CHECK (muted IN (0, 1)),
  quiet_start_min INTEGER CHECK (quiet_start_min IS NULL OR quiet_start_min BETWEEN 0 AND 1439),
  quiet_end_min   INTEGER CHECK (quiet_end_min IS NULL OR quiet_end_min BETWEEN 0 AND 1439),
  quiet_tz        TEXT    NOT NULL DEFAULT 'UTC',
  created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (user_id, org_id, event_class),
  CHECK ((quiet_start_min IS NULL) = (quiet_end_min IS NULL))
);

-- ---- audit log -----------------------------------------------------------------------------

-- Every mutation is audited (plan §4.2 "Auth"). Append-only: the triggers refuse UPDATE and
-- DELETE, so a bug in a helper cannot rewrite history. `seq` orders the log and is the cursor
-- for the audit export (a Queue job, §4.2). The org foreign key has no cascade on purpose: the
-- log outlives the org's other rows.
CREATE TABLE audit_log (
  seq             INTEGER PRIMARY KEY AUTOINCREMENT,
  id              TEXT    NOT NULL UNIQUE,
  org_id          TEXT    NOT NULL REFERENCES orgs(id),
  actor_user_id   TEXT    REFERENCES users(id),
  actor_device_id TEXT    REFERENCES devices(id),
  action          TEXT    NOT NULL,
  target_kind     TEXT    NOT NULL DEFAULT '',
  target_id       TEXT    NOT NULL DEFAULT '',
  detail_json     TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(detail_json)),
  created_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX audit_by_org ON audit_log(org_id, seq);

CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
  SELECT RAISE(ABORT, 'audit_log is append-only');
END;

CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
  SELECT RAISE(ABORT, 'audit_log is append-only');
END;

-- ---- usage rollups -------------------------------------------------------------------------

-- Daily org-wide counters written by the usage rollup job (a Cron Trigger, §4.2). `day` is
-- YYYY-MM-DD (UTC). Per-user rollups can be added later as another table.
CREATE TABLE usage_rollups (
  org_id      TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  day         TEXT    NOT NULL CHECK (day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
  metric      TEXT    NOT NULL,
  value       INTEGER NOT NULL DEFAULT 0 CHECK (value >= 0),
  updated_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (org_id, day, metric)
);
