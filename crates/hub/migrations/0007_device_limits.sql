-- M2B-005a: rate limits for device pairing (RFC 8628 §5.1 user-code brute force).
--
-- One row per counted attempt; a limit is "at most N rows for this scope in the last window".
-- Scopes (see crates/hub/src/devices/limits.rs): `approve:user:<id>` and `approve:org:<id>`
-- (failed user-code attempts), `code:ip:<CF-Connecting-IP>` (pairings started). The hub passes
-- its clock in, so the windows are testable. Old rows are purged by the cron trigger and
-- opportunistically when a pairing starts.

CREATE TABLE device_attempts (
  scope   TEXT    NOT NULL,
  at_ms   INTEGER NOT NULL
);
CREATE INDEX device_attempts_by_scope ON device_attempts(scope, at_ms);
CREATE INDEX device_attempts_by_time ON device_attempts(at_ms);
