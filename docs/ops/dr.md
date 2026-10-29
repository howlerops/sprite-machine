# Hub: disaster recovery

Scope: the production hub (`sm-hub-production` Worker, `[env.production]` of
`crates/hub/wrangler.toml`) and what it stores: D1 `sm-hub`, R2 `sm-hub-blobs` and
`sm-workspace-backups`, Durable Object SQLite storage (Thread, Machine, Flow, Workspace), queue
`sm-hub-notify`. Run commands from `crates/hub/` with `CLOUDFLARE_API_TOKEN` and
`CLOUDFLARE_ACCOUNT_ID` set. Never paste tokens or the account id into a log, a commit or a ticket.

## Targets

| Data | Mechanism | RPO | RTO |
| ---- | --------- | --- | --- |
| D1 `sm-hub` (users, orgs, devices, machines, task index) | D1 Time Travel (point in time; 30 days on the paid plan, 7 on free) plus a weekly `d1 export` to R2 | Seconds with Time Travel; up to 7 days from an export | 15 minutes (restore is in place) |
| R2 `sm-hub-blobs` (attachments) | Not versioned. Optional second copy (below) | Last object write, or last sync | Hours, manual |
| R2 `sm-workspace-backups` (Workspace DirectoryBackup) | The bucket is the backup; lifecycle rule keeps 30 days | The last `backup()` of each workspace | 30 minutes per workspace |
| Thread / Machine / Flow DO storage | No point-in-time restore for DO SQLite | The whole object history is at risk | See "Durable Object data" |
| Worker code and config | Git (`v2`) | The last commit | 15 minutes: re-run `deploy-hub` |
| Secrets | A password-manager entry per secret (names in docs/ops/hub.md) | n/a | 10 minutes: `wrangler secret put` |

These are targets, not measurements. Measure them in the drill below and record the result in the
log at the bottom.

## D1: Time Travel restore drill

Time Travel records every change. A restore rewinds the database in place and is itself undoable:
it reports the bookmark from before the restore.

1. Note the current state and a bookmark to return to:

   ```
   wrangler d1 time-travel info sm-hub --env production
   ```

2. Restore to a timestamp or a bookmark:

   ```
   wrangler d1 time-travel restore sm-hub --env production --timestamp=2026-10-29T12:00:00Z
   wrangler d1 time-travel restore sm-hub --env production --bookmark=<bookmark>
   ```

   Wrangler asks for confirmation and prints the previous bookmark. Keep it.

3. Undo a wrong restore by restoring to that previous bookmark:

   ```
   wrangler d1 time-travel restore sm-hub --env production --bookmark=<previous-bookmark>
   ```

4. Check: `wrangler d1 migrations list DB --remote --env production`, a count query with
   `wrangler d1 execute DB --remote --env production --command "SELECT count(*) FROM users"`, then
   `curl -fsS "$HUB_URL/healthz"`.

Drill (quarterly, and after a schema-changing release): do it on a throwaway database, never on
production.

```
wrangler d1 create sm-hub-drill
wrangler d1 migrations apply sm-hub-drill --remote     # with a temporary config whose DB binding points at sm-hub-drill
wrangler d1 time-travel info sm-hub-drill              # note bookmark A
wrangler d1 execute sm-hub-drill --remote --command "CREATE TABLE drill(x); INSERT INTO drill VALUES (1)"
wrangler d1 time-travel restore sm-hub-drill --bookmark=<bookmark A>
wrangler d1 execute sm-hub-drill --remote --command "SELECT name FROM sqlite_master WHERE name='drill'"   # expect no rows
wrangler d1 delete sm-hub-drill
```

Record the time from "decide" to a 200 from `/healthz` (RTO) and the gap between the incident and
the restored state (RPO). Time Travel does not survive `d1 delete`: export first.

## D1: export to R2 (outside Time Travel)

```
wrangler d1 export sm-hub --remote --env production --output=sm-hub-<utc-date>.sql
wrangler r2 object put sm-workspace-backups/d1-exports/sm-hub-<utc-date>.sql --file=sm-hub-<utc-date>.sql --remote
```

Weekly, by hand or from a scheduled workflow; keep the last 8. Restore into a new database:
`wrangler d1 create sm-hub-restore`, `wrangler d1 execute sm-hub-restore --remote --file=<file>.sql`,
put the new id in `[env.production.d1_databases]`, deploy.

## R2

- `sm-workspace-backups` is written by the Workspace DO (`BACKUPS`, prefix `backups/`). A workspace
  is restored with `restoreBackup(handle)` on the Workspace DO. Add a lifecycle rule (dashboard: R2,
  bucket, Settings, Object lifecycle rules) deleting `backups/` after 30 days.
- `sm-hub-blobs`: for a second copy, create a bucket and sync nightly with `rclone` over the R2 S3
  API using a read-only token. Without it an accidental delete cannot be undone.
- Never delete either bucket: R2 has no undelete.

## Durable Object data

DO SQLite storage has no Time Travel.

- Rebuildable: Machine presence and Flow timers come back from D1 and runner reconnects. A lost
  Thread event log is lost history, not lost accounts.
- Export: there is no export API yet (discovered work in the M2-015 report). Until then export a
  Thread through the hub API (events from `after=0`, as the org owner) and keep the JSON in R2.
  The Workspace DO exports through `backup(dir)` into `sm-workspace-backups`.
- A `deleted_classes` migration deletes that class's data for good. Never ship one without an
  export and an explicit decision in the PR.
- Recreating the Worker does not need DO data: `deploy-hub` recreates the bindings and DO ids
  reattach by name.

## Scenarios

| Scenario | Do |
| -------- | -- |
| Bad migration or corrupted D1 data | Time Travel restore to just before it; fix forward; re-deploy |
| Bad deploy | `wrangler rollback --env production` from `crates/hub`, or revert and re-run `deploy-hub`. D1 migrations are not rolled back, so write them backward compatible |
| API token leaked | Rotate it (dashboard: My Profile, API Tokens), update the repo secret `CLOUDFLARE_API_TOKEN` |
| `MASTER_KEY` lost | Per-org credentials in D1 are unreadable; orgs re-enter them. Keep it in the password manager |
| Cloudflare outage | Wait; there is no second region. cloudflarestatus.com |

## Drill log

| Date | What | RPO measured | RTO measured |
| ---- | ---- | ------------ | ------------ |
| (none yet) | | | |
