-- M2-008h (SPEC-006): one-time tickets for the browser WebSocket subscribe.
--
-- A browser cannot set an Authorization header on a WebSocket, so a session first trades its
-- bearer token for a ticket (`POST /v1/threads/{id}/subscribe-ticket`) and then opens
-- `.../subscribe?ticket=`. Only the SHA-256 of the ticket (256 random bits) is stored. The ticket
-- is bound to a session, a user, an org and one thread; it is single use (`used_at_ms`) and
-- valid for 60 seconds. `session_id` is NULL only for the `DEV_FIXTURES` dev principal, which
-- has no session. Times are epoch milliseconds: the hub passes its clock in, so expiry is testable.

CREATE TABLE subscribe_tickets (
  id            TEXT    PRIMARY KEY NOT NULL,
  ticket_hash   TEXT    NOT NULL UNIQUE CHECK (length(ticket_hash) = 64),
  session_id    TEXT    REFERENCES sessions(id) ON DELETE CASCADE,
  user_id       TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  org_id        TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  thread_id     TEXT    NOT NULL,
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL,
  used_at_ms    INTEGER
);
CREATE INDEX subscribe_tickets_by_expiry ON subscribe_tickets(expires_at_ms);
