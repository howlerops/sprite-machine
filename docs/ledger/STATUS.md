# Ledger status

_Rewritten by the leader on every ledger commit._

done 3/187 (all 3 dropped: M0-001, M0-012, M0-016) · in-flight — · blocked 10 (human: M0-018, M0-020, M0-019, M0-014, M1B-001, M1B-010, M3-016, M2B-020, M6-002, M6-006) · attempts 0/120

Ledger rev 6: **all Cloudflare**. The hub runs on Workers + Durable Objects + D1 + R2 + Queues, and managed cloud workspaces are now **Cloudflare Sandboxes** driven by a small TypeScript Workspace DO (`workers/workspace/`). There is no Fly / Sprites dependency left in v2; v1 keeps running on Sprites until M6-007 retires it.

Still from rev 5: one TurnEvent envelope inside, A2A / AG-UI only at the edges, an interim mobile web page from P2, and a `phase` on every item (plan §5). Nothing was deferred out of v2.0.

Rev 6 changes:

- Dropped: M0-001 (Sprites token), M0-012 (Sprites model connector), M0-016 (Sprites audit).
- New: **M0-020**, a model API key in Cloudflare Secrets Store (human, P4).
- Retargeted, same ids:
  - M0-008a/b, the Workspace DO and its hub binding, moved to P1;
  - M0-008c, the workspace image, P4;
  - sandbox spikes M0-009 (P2), M0-010/011/013 (P4) and M0-015 (P6);
  - M1A-010/011/012/016/017 and the M4 items.
- Backup/restore only works deployed (FUSE), so those items carry a leader-run `--env preview` verify.

## Items per phase

| Phase   | Delivers                      | Items   |
| ------- | ----------------------------- | ------- |
| P0      | Foundations                   | 12      |
| P1      | Engine                        | 25      |
| P2      | Remote Control → alpha.1      | 22      |
| P3      | Desktop app → alpha.2         | 19      |
| P4      | Managed cloud → beta.1        | 32      |
| P5      | Fleet & edges                 | 30      |
| P6      | Flows, plugins, networking    | 28      |
| P7      | Native mobile & ship → v2.0.0 | 16      |
| **All** |                               | **184** |

Open items only. The 3 dropped P4 items bring the ledger total to 187.

## P0 batch order (plan §8, week 1; needs nothing from you)

1. **M0-002** (leader): workspace skeleton with all crates (incl. `client-gpui`, `runner`, `a2a-edge`, `agui-edge`) and `web/`, `rust.yml` + `macos-latest` job, lld, profile tweaks.
2. **SPEC-001** turn-event spec + JSON Schemas, **SPEC-002** tunnel spec (leader, docs lane).
3. Workers: **M0-003** dev script (lavapipe/Vulkan + xvfb gpui screenshot, wrangler, sccache, nextest, worker-build) ‖ **M0-006** ACP types → **M1A-002** acp-mock ‖ **M0-017** Workers spike (after M0-003).
4. **MVP-000** tracer bullet (Opus): acp-mock → runner core → tunnel → hub on `wrangler dev` → headless client; `cargo run -p cli -- e2e tracer`.
5. **GPUI-000** (leader, client lane): pin gpui + gpui-component, vendor examples, write `crates/client-gpui/CONVENTIONS.md`; **M1B-003** desktop gates (leader, docs lane).
6. Workers: **M1B-002** TurnEvent fixture server (after MVP-000) → **M1B-004a** gpui spike (client lane, after the lavapipe proof and GPUI-000).

Then P1 opens with **M3-001** client-core (after SPEC-001 + MVP-000), M1A-001/003, M2-001/016, M2B-001, and **M0-008a** (TypeScript Workspace DO skeleton, then **M0-008b** binds it into the hub; both run locally under `wrangler dev --enable-containers=false` with a fake sandbox). From P1 on, every item replaces a tracer stub and `cli e2e tracer` must stay green after every merge.

## Waiting on the human

Nothing blocks P0 or P1. The asks, in the order to provide them:

| Item    | Phase | Needed                                                                                                                                                                                                                                                              |
| ------- | ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| M0-018  | P2    | Cloudflare account (Workers Paid) + API token (Workers Scripts, Containers, D1, R2, Queues, Secrets Store edit; Workers Tail + Account Settings + Account Analytics read) as `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID` env secrets, then restart the session |
| M2B-020 | P2    | Dogfood Remote Control on your laptop + phone browser after the hub is deployed                                                                                                                                                                                     |
| M0-019  | P3    | GitHub Actions `macos-latest` runners enabled for the repo (or a self-hosted Mac runner)                                                                                                                                                                            |
| M0-020  | P4    | A model API key (e.g. Anthropic) in Cloudflare Secrets Store; tell the leader only the secret name                                                                                                                                                                  |
| M0-014  | P6    | `TS_AUTHKEY` + `WG_TEST_PEER` env secrets (optional; networking spike)                                                                                                                                                                                              |
| M1B-001 | P7    | Apple Developer account, iPhone, Android phone, Mac / macOS runners                                                                                                                                                                                                 |
| M1B-010 | P7    | Run the mobile gates on physical devices                                                                                                                                                                                                                            |
| M3-016  | P7    | Mobile on-device acceptance (push tap, a11y, offline)                                                                                                                                                                                                               |
| M6-002  | P7    | Cloudflare account-scoped deploy token for promotion to Workers/Containers                                                                                                                                                                                          |
| M6-006  | P7    | Signing certs/keys and store listings                                                                                                                                                                                                                               |
