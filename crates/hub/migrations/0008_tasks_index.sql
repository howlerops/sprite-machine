-- M2-010: the task index carries what `GET /v1/threads` lists and filters on (machine, agent,
-- workspace path, the last event seq) and the soft delete. The Thread DO upserts the row after
-- each change to a thread's title, status or lifecycle (`tasks::sync`).
--
-- `status` keeps the 0002 values (open, running, waiting, done, failed, cancelled). The client
-- API names them idle, running, needs_you and done (client-api.md §5.2), so no CHECK changes.
-- A deleted thread (DELETE /v1/threads/{id}) keeps its row with `deleted_at` set.
ALTER TABLE tasks ADD COLUMN machine_id     TEXT    NOT NULL DEFAULT '';
ALTER TABLE tasks ADD COLUMN agent          TEXT    NOT NULL DEFAULT '';
ALTER TABLE tasks ADD COLUMN workspace_path TEXT    NOT NULL DEFAULT '';
ALTER TABLE tasks ADD COLUMN last_seq       INTEGER NOT NULL DEFAULT 0 CHECK (last_seq >= 0);
ALTER TABLE tasks ADD COLUMN deleted_at     TEXT;
CREATE INDEX tasks_by_machine ON tasks(org_id, machine_id, updated_at);
