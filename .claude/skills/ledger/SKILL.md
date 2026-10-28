---
name: ledger
description: Run a body of work to completion with a ledger + leader/worker loop. An Opus leader keeps a ledger of work items, assigns ready items to Haiku workers (Opus for risky ones) in parallel git worktrees, reviews every result against its acceptance criteria, merges, and loops until every item is done or waiting on a human. Use when asked to "work the ledger", "run the ledger", "execute the plan/milestone with workers", "fan out workers", to turn a plan into a ledger, to add/split/re-plan items, or to report ledger status / what is blocked on the human. Not for financial ledgers.
---

# Ledger: leader / worker execution

You are the **leader**. You own the ledger, assign work, review it, merge it, and
decide when it is done. **Workers** are subagents that each implement exactly one
item and report back. Only the leader edits the ledger, merges, and marks items done.

## Roles and models

- **The leader must be the top-level session, running Opus.** Subagents cannot spawn
  subagents. If this session is not Opus, stop and tell the user to restart with
  `--model opus` (or `/model opus`).
- **Workers**: the `Agent` tool with `isolation: "worktree"` and `run_in_background: true`.
  - **Model** (cost policy set by the user, 2026-10-28): `haiku` by default. Use `sonnet` on attempt 1 when the item is `risk: high` or `size: L`, and on attempt 2+ of any item. Workers never run on `opus`; only the leader does. If a sonnet attempt 3 fails, split the item or block it.
  - One worker per item, one item per worker.

## Files

| File                          | Purpose                                                                      |
| ----------------------------- | ---------------------------------------------------------------------------- |
| `docs/ledger/LEDGER.yaml`     | The ledger. Format: [reference/ledger-format.md](reference/ledger-format.md) |
| `docs/ledger/STATUS.md`       | Human-readable status, rewritten on every ledger commit                      |
| `docs/ledger/LOG.md`          | Append-only: `<UTC time> <ID> <from>→<to> <note>`                            |
| `docs/ledger/reports/<ID>.md` | Worker reports, committed on the worker branch                               |

If no ledger exists, build one first (see Planning), then run the loop.

## The loop

There are no fixed waves. Keep up to `max_parallel` workers in flight, and refill a
slot as soon as one frees up.

**0. Reconcile** (at loop start, and after any restart or context reset):

- `git pull` the working branch and re-read the ledger.
- For each `assigned` / `review` item with no live worker in this session:
  - if a branch matching `*<ID>*` has commits beyond `branch`, review it as a result;
  - otherwise reset it to `todo` (keep `attempt`) and log `orphaned`.
- Unblock human items whose condition now holds. For example, `test -n "$SPRITES_API_TOKEN"` flips M0-001 to `done` with evidence. New environment secrets only appear after a session restart.

**1. Pick** ready items:

- `status: todo` and `owner: worker`;
- every dep `done`;
- `paths` (plus implicit paths, see the format doc) do not overlap any in-flight item.

Order by `phase` (header `phase_order`), then `milestone` (`milestone_order`), then
`priority`. Work in the current phase comes first. Items from a later phase are
picked only when nothing in the current phase is ready, and never when they would
starve an earlier phase's lane.

**Lanes**: each item has a `lane` (default `rust`). The header `lanes` caps how
many items per lane are in flight. The `client` lane (GPUI) is capped at 1, so
GPUI builds never compete.

Do `owner: leader` items yourself between assignments. Leader-owned spec items
(`SPEC-*`) come before any worker item that implements them.

**2. Assign** each picked item:

1. Set `status: assigned`, `attempt += 1`, `assignee`.
2. Commit the ledger.
3. Spawn the worker with the brief from [reference/worker-brief.md](reference/worker-brief.md).

Spawn all assignments of one pick in a **single message**. Do not poll: you are
re-invoked when each worker finishes.

**3. Review** each result as it arrives, using
[reference/review-checklist.md](reference/review-checklist.md). The worker's reply
starts with its branch name and HEAD SHA.

- **Pass**: merge (see checklist), then `status: done`, `evidence: "merged <sha>; verify: <n> passed, clippy clean"`.
- **Fail**: `status: changes` with numbered `review_notes`, then re-assign (attempt+1).
- **After attempt 3 fails**: split the item, or set `blocked` with `blocked_on`.
- **A merge conflict that isn't only `Cargo.lock`**: set `changes` with the note "merge v2 @<sha> into your branch and re-run verify". This does **not** count as an attempt.

**4. Re-plan**:

- Promote "discovered work" from reports into new items.
- Split items that turned out too big.
- Never silently widen an item.

**5. Report**:

- After every assignment batch and every merge, rewrite `STATUS.md` and append to `LOG.md`.
- Tell the user one line: `done 12/130 · in-flight M0-003b,M0-005a · blocked 6 (human: M0-001, …) · attempts 19/250`.

## Stop conditions

Stop and summarize when any of these holds:

1. Every item is `done`.
2. Every remaining non-done item is `owner: human`, `blocked`, or transitively depends on one of those.
   - Set the dependents `blocked` with `blocked_on: <human item id>`.
   - Post **one** message listing each human item's `howto`.
   - Stop. Do not idle.
3. **Budget**: total attempts reach `budget.max_total_attempts`, or three consecutive merges-or-rejections make no progress (nothing merged).
4. The user says stop.

**Timeouts**: if a worker hasn't returned within `budget.max_worker_minutes`, stop
it (TaskStop), record `timeout` in its review_notes, and count the attempt.

## Leader rules

- **Review is the job.** A worker's "done" means nothing until you've seen the diff and re-run `verify` yourself.
- **Keep items worker-sized**:
  - one module/screen, ≲300 changed lines;
  - explicit paths and ≥2 `context` entries (a plan section + an upstream spec or sibling code);
  - runnable `verify` that fails without the change.

  If you can't write such a `verify`, the item isn't ready: split it or add a spike.

- **Prefer integration-test targets** (`cargo test -p x --test name`) over name filters. Name filters pass vacuously when zero tests match, so the checklist counts tests.
- **Never** accept skipped, ignored or weakened tests or lints.
- **Secrets** come only from the environment. Never put them in briefs, reports, the ledger, service env, or logs.
- **Live-infra verifies** (Sprites, Fly) run **once per attempt**, and always clean up what they create (`SMOKE_KEEP=0`).
- **Review items** (`*-review`): the leader's report quotes each plan exit criterion next to the command and output that demonstrate it. Review items never gate other items.
- **Keep the working branch green.** If a merge breaks the workspace checks, fixing that becomes the next item, at priority 0.
- **The tracer bullet is the integration test.** Once MVP-000 merges, `cargo run -p cli -- e2e tracer` runs after every merge. Each later item replaces a stub inside it, and the tracer must stay green.
- **Phase reviews**: when a phase's items are done, tag `v2.0.0-alpha.N` (or beta/rc per plan §5) and tell the user what they can now try and how.
- **Commits**: `ledger(<ID>): <title>` for work; `ledger: <change>` for ledger edits.

## Planning (turning a plan into a ledger)

1. Read the plan and the repo, and check the container's tooling (`which cargo flutter docker xcodebuild`).
2. Break each milestone into worker-sized items per the format doc. Pre-create shared scaffolding (module stubs, `mod.rs` files) in an early leader item, so parallel items only add files.
3. Human-only work (accounts, tokens, devices, store, product decisions) gets `owner: human`, `status: blocked`, `blocked_on: human`, and a precise `howto`.
4. Add a `<M>-review` leader item per milestone.
5. Validate: unique ids, deps exist, no cycles. Commit before running the loop.
