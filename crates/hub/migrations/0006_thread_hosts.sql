-- M2-008e: which org owns each thread (client-api.md §2: "a thread is visible to members of
-- the org that owns its machine").
--
-- Written whenever a thread's host is recorded: `POST /v1/threads` (org of the caller, after
-- the machine check), a runner `thread_open` via the Machine DO (org of that machine), and the
-- DEV_FIXTURES init route (org of the dev principal). The first writer wins; a thread id
-- already mapped to another org is refused. The Worker consults this table on every
-- `/v1/threads/{id}/...` route: no row, or another org's row, is `404 not_found`.
--
-- No foreign keys: dev machines have no `machines` row, and the table must not decide which
-- orgs or machines may exist.

CREATE TABLE thread_hosts (
  thread_id  TEXT PRIMARY KEY NOT NULL,
  org_id     TEXT NOT NULL,
  machine_id TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX thread_hosts_by_org ON thread_hosts(org_id, created_at);
