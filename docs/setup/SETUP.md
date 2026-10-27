# Environment setup for the v2 ledger run

Do steps 1–3 now. Steps 4–5 only need doing before the phase that uses them.

## 1. Cloud environment → Setup script (2 min)

Open the cloud environment menu in the session title bar, choose **Edit**, then
**Setup script**. Paste the contents of [`scripts/cloud-setup.sh`](../../scripts/cloud-setup.sh).

It is idempotent and needs no secrets. It installs:

- the Vulkan software renderer (lavapipe), so GPUI can render headless;
- X/Wayland dev libraries and musl;
- Rust targets: musl, wasm32 and wasm32-wasip2;
- sccache, cargo-nextest, worker-build and wrangler.

It was tested in this container: about 3.5 minutes, ending with `setup complete`
and a Vulkan device of `llvmpipe`.

## 2. Cloud environment → Network access

The current level already reaches everything P0–P2 needs. If you ever switch to
**Limited**, keep _Allow package managers_ ticked and add these under Allowed domains:

- **Rust / crates:** `static.crates.io`, `index.crates.io`, `static.rust-lang.org`, `sh.rustup.rs`
- **GitHub (git dependencies, releases):** `github.com`, `objects.githubusercontent.com`, `raw.githubusercontent.com`, `codeload.github.com`
- **Dev tools:** `get.nexte.st`, `registry.npmjs.org`
- **Cloudflare:** `api.cloudflare.com`, `*.workers.dev`
- **Sprites:** `api.sprites.dev`, `*.sprites.app`
- **Ubuntu packages:** `archive.ubuntu.com`, `security.ubuntu.com`

## 3. Repo permission rules (stop prompts during the ledger run)

Review [`docs/setup/claude-settings.proposed.json`](claude-settings.proposed.json).

If you're happy with it:

1. Copy it to `.claude/settings.json`.
2. Add `!.claude/settings.json` to `.gitignore`, right after the `!.claude/skills/` line.
3. Commit it.

The file allows cargo, wrangler (dev only), npm/playwright, the repo's scripts and
non-destructive git on `v2`. It denies force-pushes and `wrangler delete`.

Also pick **Auto** (or **Accept edits**) in the mode dropdown next to the prompt
box when you start the ledger run.

## 4. Secrets: before P2 (Cloudflare) and P4 (Sprites)

Add these under the environment's **Network secrets** (or as environment variables).
A new session picks them up. Never paste them into chat.

| Variable                     | Needed by     | How to get it                                                                                                                                                                                                                                                                                                     |
| ---------------------------- | ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CLOUDFLARE_ACCOUNT_ID`      | P2 (M0-018)   | Cloudflare dashboard → Workers & Pages → Account ID (right sidebar). Use a **Workers Paid** plan ($5/month).                                                                                                                                                                                                      |
| `CLOUDFLARE_API_TOKEN`       | P2 (M0-018)   | Dashboard → My Profile → API Tokens → Create Token → Custom token, scoped to that one account. Permissions (all **Account**): Workers Scripts Edit, D1 Edit, Workers R2 Storage Edit, Queues Edit, Workers Tail Read, Account Settings Read. Add _Zone → Workers Routes Edit_ only if you'll use a custom domain. |
| `SPRITES_API_TOKEN`          | P4 (M0-001)   | Run `sprite login` then `sprite org auth` locally, or use sprites.dev → account → tokens. **Restrict it to name prefix `sm-`** with a creation cap (e.g. 20).                                                                                                                                                     |
| `TS_AUTHKEY`, `WG_TEST_PEER` | P6 (optional) | An ephemeral, pre-authorized Tailscale key, and a WireGuard peer config with a hostname endpoint.                                                                                                                                                                                                                 |

`FLY_TOKEN` (already set) is **no longer used for the hub**. Keep it only if you
want "promote to Fly" in P7. Ideally replace it then with an app-scoped deploy
token (`fly tokens create deploy -a <app>`).

## 5. Later asks

| When | What                                                                                                                 |
| ---- | -------------------------------------------------------------------------------------------------------------------- |
| P3   | GitHub Actions with `macos-latest` enabled for this repo, for unsigned macOS desktop builds you can run on your Mac. |
| P4   | A model-provider connector in the Sprites org (see M0-012 in the ledger).                                            |
| P7   | An Apple Developer account, an iPhone and an Android phone, and signing material for store and desktop releases.     |

## 6. Start the run

1. Start a **new session** on this repo with the **Opus** model, so it picks up the setup script and any secrets.
2. Choose **Auto** mode.
3. Say: **"work the ledger"**.

The leader follows `.claude/skills/ledger/SKILL.md`:

1. It does M0-002 itself.
2. It writes the two specs.
3. It fans out the P0 workers.

**Following progress**:

- `docs/ledger/STATUS.md` and `LOG.md` on the `v2` branch;
- a one-line update after each batch;
- an `alpha.N` tag at each phase review.

**Resuming**: if the session ends or its context fills up, start a new one and say
"work the ledger" again. The leader's _Reconcile_ step picks up where it left off
from the committed ledger and the workers' branches.
