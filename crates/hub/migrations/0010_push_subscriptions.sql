-- M2-011: push subscriptions and per-thread mutes for the notification fan-out (plan §4.2
-- "Notifications"). The class, mute and quiet-hours prefs themselves live in
-- `notification_prefs` (0002); this migration adds what that table has no room for.
--
-- A subscription belongs to a user and to the device that registered it (the session's device),
-- in one org. `provider` says how it is delivered: `webpush` (the mobile web page / PWA;
-- `endpoint` is the push service URL, `p256dh` and `auth` the subscription keys, both base64url),
-- `apns` or `fcm` (native apps, P3/P7; `endpoint` is the device token, no keys). `endpoint` is
-- unique across the table: registering the same endpoint again updates the row (idempotent),
-- also when another user signs in on the same browser. The endpoint is a capability URL, so it
-- is never logged or audited.

CREATE TABLE push_subscriptions (
  id          TEXT PRIMARY KEY NOT NULL,
  org_id      TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  device_id   TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
  provider    TEXT NOT NULL CHECK (provider IN ('webpush', 'apns', 'fcm')),
  endpoint    TEXT NOT NULL UNIQUE,
  p256dh      TEXT,
  auth        TEXT,
  created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  CHECK ((provider = 'webpush') = (p256dh IS NOT NULL AND auth IS NOT NULL))
);
CREATE INDEX push_subscriptions_by_org_user ON push_subscriptions(org_id, user_id);

-- Threads a user muted (all event classes). A row means muted.
CREATE TABLE notification_thread_mutes (
  user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  org_id     TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  thread_id  TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (user_id, org_id, thread_id)
);
