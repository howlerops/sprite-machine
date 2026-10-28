-- M2B-005: device-code pairing (RFC 8628) and trusted devices (plan §4.10).
--
-- `sm login` starts a pairing (`POST /v1/devices/code`); a member approves it
-- (`POST /v1/devices/approve`); the runner then trades its device code for a machine id and an
-- `smm_` credential, once (`POST /v1/devices/token`). Only the SHA-256 of the device code is
-- stored; the user code is short-lived and typed by a human, so it is stored as is.
--
-- A paired machine is an ordinary `machines` row (the registry of 0004). `origin` says how it
-- came to be: 'admin' (POST /v1/machines, the default for every existing row) or 'device' (an
-- approved pairing = a trusted device). `orgs.require_trusted_devices` (0002) is the org
-- setting. Admin-API machines stay allowed when it is on; see crates/hub/src/devices/trust.rs.

ALTER TABLE machines ADD COLUMN origin TEXT NOT NULL DEFAULT 'admin' CHECK (origin IN ('admin', 'device'));
ALTER TABLE machines ADD COLUMN approved_by_user_id TEXT REFERENCES users(id);
ALTER TABLE machines ADD COLUMN approved_at TEXT;

CREATE TABLE device_pairings (
  id                      TEXT    PRIMARY KEY NOT NULL,
  device_code_hash        TEXT    NOT NULL UNIQUE CHECK (length(device_code_hash) = 64),
  -- 8 characters of the unambiguous alphabet, upper case, no dash.
  user_code               TEXT    NOT NULL UNIQUE,
  -- pending -> approved | denied; approved -> consumed (the credential was handed out once).
  status                  TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'denied', 'consumed')),
  -- epoch milliseconds (the hub passes its clock in, so the rules are testable).
  created_at_ms           INTEGER NOT NULL,
  expires_at_ms           INTEGER NOT NULL,
  interval_seconds        INTEGER NOT NULL CHECK (interval_seconds > 0),
  last_poll_at_ms         INTEGER,
  org_id                  TEXT    REFERENCES orgs(id) ON DELETE CASCADE,
  approved_by_user_id     TEXT    REFERENCES users(id),
  approved_by_device_id   TEXT    REFERENCES devices(id),
  machine_name            TEXT,
  machine_id              TEXT
);
CREATE INDEX device_pairings_by_expiry ON device_pairings(expires_at_ms);
