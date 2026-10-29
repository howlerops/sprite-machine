# Hub: Cloudflare resources

Status: **provisioned 2026-10-29** (M2-014) by the leader, after the user allowed the exact commands in `.claude/settings.json`. The leader's attempt to create these from the cloud
session was refused by the session's permission check (creating resources in a shared
Cloudflare account), so a human runs the commands below, or allows the leader to.

The Cloudflare account (Howlerops) already holds resources of other projects
(`laurabeckart-db`, buckets `slate-explorer`, `trigon-models`). Everything here is new and
prefixed `sm-hub`; nothing existing is touched.

## Resources

| Resource | Name            | Binding (crates/hub/wrangler.toml) | Command                                    |
| -------- | --------------- | ---------------------------------- | ------------------------------------------ |
| D1       | `sm-hub`        | `DB`                               | `wrangler d1 create sm-hub`                |
| R2       | `sm-hub-blobs`  | `BLOBS`                            | `wrangler r2 bucket create sm-hub-blobs`   |
| Queue    | `sm-hub-notify` | `NOTIFY` (producer + consumer)     | `wrangler queues create sm-hub-notify`     |

Run from `crates/hub/` with `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` set. Then put
the D1 `database_id` printed by `d1 create` into `wrangler.toml` under `[env.production]`
(the leader does this once the id is known: `wrangler d1 list --json`).

## Provisioned

| Resource | Name | Id / note |
| -------- | ---- | --------- |
| D1 | `sm-hub` | `database_id = "6b65194f-4317-471b-a718-f89d8fdcfbeb"` (region ENAM) |
| R2 | `sm-hub-blobs` | hub attachments (`BLOBS`) |
| R2 | `sm-workspace-backups` | Workspace DO backups (`workers/workspace`, `BACKUPS`) |
| Queue | `sm-hub-notify` | push notifications (`NOTIFY` producer + consumer) |

The `[env.production]` section of `crates/hub/wrangler.toml` that binds these (bindings are not
inherited by Wrangler environments) is in place (M2-015).

## Deploy (M2-015)

`[env.production]` in `crates/hub/wrangler.toml` binds the resources above (Worker name
`sm-hub-production`). [`.github/workflows/deploy-hub.yml`](../../.github/workflows/deploy-hub.yml) runs on a
push to `v2` that touches `crates/hub/`, `crates/protocol/`, `web/` or the workflow, and on manual
dispatch: build cli and the hub (`wasm-release`), `wrangler d1 migrations apply DB --remote --env production`,
`wrangler deploy --env production`, then `GET /healthz` must be 200.

Set in the repo (Settings, Secrets and variables, Actions); names only here:

- Secrets: `CLOUDFLARE_API_TOKEN` (Workers Scripts, D1, Queues edit on the account),
  `CLOUDFLARE_ACCOUNT_ID`. Without them the job prints a notice and ends green, deploying nothing.
- `HUB_URL` (variable or secret): the hub's https origin, for the health check. Unset: the check is skipped.

Before the first hub deploy, deploy the Workspace DO Worker by hand: the hub's `WORKSPACE` binding
points at the script `sm-workspace` (`workers/workspace`), and a deploy that references a missing
script fails. `workers/workspace/wrangler.jsonc` has no `env.production` and needs its container
image built (M0-008c), so the workflow does not deploy it: `cd workers/workspace && npm ci && npx wrangler deploy`.
Redeploy it when its code changes.

Then set the secrets below. Recovery: [dr.md](dr.md). Alerts: [alerts.md](alerts.md).

## Secrets (after the first deploy, M2-014a)

Worker secrets need the Worker to exist, and the hub is deployed by CI (M2-015). After the
first deploy:

```
wrangler secret put MASTER_KEY --env production            # 32 random bytes, base64; envelope key for per-org credentials in D1
wrangler secret put BOOTSTRAP_OWNER_TOKEN --env production # you choose it; used once to claim the owner account
wrangler secret put VAPID_PRIVATE_KEY --env production     # Web Push; the public key goes in [env.production.vars]
```

Var `BOOTSTRAP_OWNER_GITHUB_LOGIN` (optional, M2-003f): set it to the GitHub login of the first owner in `[env.production.vars]`; that owner's first GitHub sign-in then makes them the owner, however old their account is. Once they have signed in, the var does nothing more.

Generate the master key with `openssl rand -base64 32`. Never paste any of these into chat,
a commit, a log or the ledger.
