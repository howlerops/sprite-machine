-- M2-003b: GitHub OAuth (authorization code + PKCE) and org invites.
--
-- users.github_id is the stable GitHub account id: a hub user is mapped by it, never by the
-- login (a login can be renamed and re-used by someone else). github_login stays as the display
-- and invite handle and is refreshed on every sign-in.
--
-- oauth_requests: one row per /v1/auth/github/authorize call. Only the SHA-256 of the `state`
-- sent to GitHub is stored. `github_verifier` is the PKCE verifier of the hub<->GitHub leg (it
-- is only useful together with the GitHub code, and the row is single use and short lived).
-- `app_*` is the app<->hub leg: the allowlisted redirect URI, the app's S256 challenge and its
-- own opaque `state`, echoed back. oauth_codes: the hub's one-time authorization code handed to
-- the app, redeemed at POST /v1/auth/github/token with the app's verifier. Only its hash is kept.
-- org_invites: an admin invites a GitHub login (stored lowercase); the invite is accepted on the
-- first sign-in by that account. Times are epoch milliseconds (the hub passes its clock in).

ALTER TABLE users ADD COLUMN github_id INTEGER;
CREATE UNIQUE INDEX users_by_github_id ON users(github_id) WHERE github_id IS NOT NULL;

CREATE TABLE oauth_requests (
  id               TEXT    PRIMARY KEY NOT NULL,
  state_hash       TEXT    NOT NULL UNIQUE CHECK (length(state_hash) = 64),
  github_verifier  TEXT    NOT NULL,
  app_redirect_uri TEXT    NOT NULL,
  app_challenge    TEXT    NOT NULL CHECK (length(app_challenge) = 43),
  app_state        TEXT    NOT NULL,
  org_id           TEXT    REFERENCES orgs(id) ON DELETE CASCADE,
  created_at_ms    INTEGER NOT NULL,
  expires_at_ms    INTEGER NOT NULL,
  consumed_at_ms   INTEGER
);
CREATE INDEX oauth_requests_by_expiry ON oauth_requests(expires_at_ms);

CREATE TABLE oauth_codes (
  id             TEXT    PRIMARY KEY NOT NULL,
  code_hash      TEXT    NOT NULL UNIQUE CHECK (length(code_hash) = 64),
  user_id        TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  org_id         TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  redirect_uri   TEXT    NOT NULL,
  challenge      TEXT    NOT NULL CHECK (length(challenge) = 43),
  invite_ids     TEXT    NOT NULL DEFAULT '',
  created_at_ms  INTEGER NOT NULL,
  expires_at_ms  INTEGER NOT NULL,
  used_at_ms     INTEGER
);
CREATE INDEX oauth_codes_by_expiry ON oauth_codes(expires_at_ms);

CREATE TABLE org_invites (
  id                  TEXT    PRIMARY KEY NOT NULL,
  org_id              TEXT    NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  github_login        TEXT    NOT NULL CHECK (github_login = lower(github_login) AND length(github_login) BETWEEN 1 AND 39),
  role                TEXT    NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'viewer')),
  invited_by_user_id  TEXT    REFERENCES users(id) ON DELETE SET NULL,
  created_at_ms       INTEGER NOT NULL,
  expires_at_ms       INTEGER NOT NULL,
  accepted_at_ms      INTEGER,
  accepted_user_id    TEXT    REFERENCES users(id) ON DELETE SET NULL,
  revoked_at_ms       INTEGER
);
-- One open invite per (org, login).
CREATE UNIQUE INDEX org_invites_open ON org_invites(org_id, github_login)
  WHERE accepted_at_ms IS NULL AND revoked_at_ms IS NULL;
CREATE INDEX org_invites_by_login ON org_invites(github_login);
