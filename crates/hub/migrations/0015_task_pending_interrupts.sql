-- M2-010h: the task index carries the thread's open approval interrupts, so `GET /v1/threads`
-- can list them on each `needs_you` row without a Thread DO round trip per row. JSON array of
-- {interrupt_id, title, options: [{option_id, name, kind}]}, written by the Thread DO in the
-- same upsert as the status (an interrupt opening or being answered already refreshes the row).
ALTER TABLE tasks ADD COLUMN pending_interrupts TEXT NOT NULL DEFAULT '[]';
