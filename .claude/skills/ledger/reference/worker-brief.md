# Worker brief template

Fill this in for each assignment. Pass it as the `Agent` prompt with
`isolation: "worktree"` and `run_in_background: true`. Use `model: "haiku"` by
default, and `"opus"` for `risk: high` / `size: L` items or attempt 3.

The brief is the worker's **only** context, so paste the item's fields verbatim.

```
You are a worker on ledger item {ID} (attempt {attempt}). Your git worktree is
already on its own branch, created from `{branch}`. Implement exactly this item,
nothing else.

## Item
Title: {title}
Read first: {context}
You may change only: {paths}
Also allowed: this crate's Cargo.toml, module declarations / route registration in
lib.rs/main.rs/mod.rs, new files under the crate's tests/, Cargo.lock, and
docs/ledger/reports/{ID}.md.

## Acceptance criteria (all must hold)
{acceptance}

## Verify (run these; all must exit 0 before you report)
{verify}

## Previous review notes (fix every one)
{review_notes or "none"}

## Environment
export CARGO_TARGET_DIR={env.CARGO_TARGET_DIR}

## Rules
- Do not edit docs/ledger/LEDGER.yaml, STATUS.md or LOG.md.
- Do not skip, ignore or weaken tests or lints. Every acceptance criterion needs
  a test that would fail without your change.
- If a criterion is wrong or impossible, or a verify command needs a secret or
  tool that is missing, stop and say so in your report (with `blocked_on`).
  Don't work around it.
- Do not run live Sprites/Fly commands except those in your verify list.
- No secrets in code, logs or the report.
- Match the surrounding code style; keep the change minimal.
- Run `cargo fmt --all` before committing.
- Commit with message `ledger({ID}): {title}`.

## Report
Write docs/ledger/reports/{ID}.md and commit it:
- Summary (≤5 lines)
- Files changed
- Verify output: each command, pass/fail, and the last lines
- Each acceptance criterion: met / not met, with the test name or output that shows it
- Discovered work, written as proposed ledger items (YAML)

Your final reply MUST start with these two lines:
BRANCH: <output of `git branch --show-current`>
SHA: <output of `git rev-parse HEAD`>
followed by the report contents.
```
