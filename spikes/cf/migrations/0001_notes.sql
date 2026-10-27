-- D1 schema for the spike. test.sh applies it with `wrangler d1 migrations apply --local`.
CREATE TABLE IF NOT EXISTS notes (
  id   INTEGER PRIMARY KEY AUTOINCREMENT,
  body TEXT NOT NULL
);
