-- M2-001: placeholder so `wrangler d1 migrations apply DB` has a migration to run.
-- The hub's real D1 tables (plan §4.2: users, orgs, devices, machines, ...) arrive with the
-- items that need them.
CREATE TABLE IF NOT EXISTS hub_placeholder (
  id INTEGER PRIMARY KEY NOT NULL
);
