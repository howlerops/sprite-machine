# Worker brief template

Fill this in for each assignment and pass it as the `Agent` prompt (`model: "haiku"`,
`isolation: "worktree"`; `model: "opus"` for escalations). The brief is the worker's
only context, so be explicit.

```
You are a worker on ledger item {ID} (attempt {attempt}) in the repo at the working
directory, branch from `{branch}`. Implement exactly this item, nothing else.

## Item
Title: {title}
Read first: {context}
You may change only: {paths} (plus docs/ledger/reports/{ID}.md)

## Acceptance criteria (all must hold)
{acceptance}

## Verify (run these; all must exit 0 before you report)
{verify}

## Previous review notes (fix all of these)
{review_notes or "none"}

## Rules
- Do not edit docs/ledger/LEDGER.yaml or LOG.md.
- Do not skip, disable or weaken tests or lints. If a criterion seems wrong or
  impossible, stop and say so in your report instead of working around it.
- No secrets in code, logs or the report.
- Follow the surrounding code style. Keep the change minimal.
- Commit on your branch with message `ledger({ID}): {title}`.

## Report
Write docs/ledger/reports/{ID}.md and commit it:
- Summary (≤5 lines) of what changed and why
- Files changed
- Verify output (command + pass/fail + last lines)
- Each acceptance criterion: met / not met + how you know
- Open issues or discovered work (as proposed new ledger items)
Then reply with: your branch name, the commit SHA, and the report contents.
```
