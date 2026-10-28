-- M2-003c: browser sign-in (client-api.md §5.7).
--
-- A: a paired machine mints a one-time sign-in link (`POST /v1/devices/signin-links`); the page
-- redeems it (`POST /v1/auth/links/redeem`). Only the SHA-256 of the link code is stored.
-- B: a browser runs an RFC 8628 device-code flow for a user session. Its own table, so a
-- machine pairing's device code can never be traded for a session (or the other way round).
-- Times are epoch milliseconds: the hub passes its clock in, so expiry is testable.
-- A session may carry `role_cap`: the effective role is the lower of the member's role and it.

ALTER TABLE sessions ADD COLUMN role_cap TEXT CHECK (role_cap IS NULL OR role_cap IN ('owner', 'admin', 'member', 'viewer'));

CREATE TABLE signin_links (
  id             TEXT    PRIMARY KEY NOT NULL,
  code_hash      TEXT    NOT NULL UNIQUE CHECK (length(code_hash) = 64),
  org_id         TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  user_id        TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  machine_id     TEXT    NOT NULL,
  created_at_ms  INTEGER NOT NULL,
  expires_at_ms  INTEGER NOT NULL,
  used_at_ms     INTEGER
);
CREATE INDEX signin_links_by_machine ON signin_links(machine_id, expires_at_ms);
CREATE INDEX signin_links_by_expiry ON signin_links(expires_at_ms);

CREATE TABLE browser_pairings (
  id                  TEXT    PRIMARY KEY NOT NULL,
  device_code_hash    TEXT    NOT NULL UNIQUE CHECK (length(device_code_hash) = 64),
  user_code           TEXT    NOT NULL UNIQUE,
  -- pending -> approved; approved -> consumed (the session token was handed out once).
  status              TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'consumed')),
  created_at_ms       INTEGER NOT NULL,
  expires_at_ms       INTEGER NOT NULL,
  interval_seconds    INTEGER NOT NULL CHECK (interval_seconds > 0),
  last_poll_at_ms     INTEGER,
  org_id              TEXT    REFERENCES orgs(id) ON DELETE CASCADE,
  approved_by_user_id TEXT    REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX browser_pairings_by_expiry ON browser_pairings(expires_at_ms);
