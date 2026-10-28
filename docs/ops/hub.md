# Hub: Cloudflare resources

Status: **not provisioned yet** (M2-014). The leader's attempt to create these from the cloud
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

## Secrets (after the first deploy, M2-014a)

Worker secrets need the Worker to exist, and the hub is deployed by CI (M2-015). After the
first deploy:

```
wrangler secret put MASTER_KEY --env production            # 32 random bytes, base64; envelope key for per-org credentials in D1
wrangler secret put BOOTSTRAP_OWNER_TOKEN --env production # you choose it; used once to claim the owner account
wrangler secret put VAPID_PRIVATE_KEY --env production     # Web Push; the public key goes in [env.production.vars]
```

Generate the master key with `openssl rand -base64 32`. Never paste any of these into chat,
a commit, a log or the ledger.
