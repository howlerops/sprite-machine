# Leader review checklist

Run this for every worker result. `B` is the worker's branch, `W` is the working
branch.

## 1. Check out for review without disturbing your own checkout

```
git worktree add --detach /tmp/review-<ID> B   # detached: B is still checked out in the worker's worktree
```

Work inside `/tmp/review-<ID>`, with `CARGO_TARGET_DIR` set to the shared target dir.

## 2. Scope

`git diff W...B --stat` touches only the item's `paths` plus the implicit paths in
the format doc. Anything else is a reject.

## 3. Verify

Re-run every `verify` command yourself.

- For a name-filtered `cargo test -p <crate> <filter>`, also run `cargo test -p <crate> <filter> -- --list 2>/dev/null | grep -c ': test$'`. A count below `min_tests` (default 3) is a reject; 0 means the verify passed vacuously.
- Live-infra commands run once per attempt.

## 4. Acceptance

Check each criterion against the diff and the test output, not the report's
claims. Each criterion needs a test, or a command whose output shows it.

## 5. Tests are real

- No `#[ignore]`, `skip`, `todo!()`, commented-out asserts, or loosened lints (`allow(...)` added to pass clippy).
- New behaviour has a test that would fail without the change.

## 6. Correctness read (adversarial)

- swallowed errors;
- `unwrap`/`expect` on network or IO;
- async races and cancellation;
- protocol fields checked against the spec (A2A 1.0 / ACP v1 / AG-UI 1.0);
- secrets in code, logs or env;
- path traversal;
- SSRF.

## 7. Fit

Matches the plan section and the surrounding code. No gold-plating.

## 8. Merge (on W, in your main checkout)

```
git merge --no-ff --no-commit B
# If only Cargo.lock conflicts:
git checkout --ours Cargo.lock && cargo update --workspace && git add Cargo.lock
# Any other conflict:
git merge --abort   # → status: changes, "merge W @<sha> into your branch and re-run verify" (attempt not counted)
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
git commit -m "ledger(<ID>): <title>"     # only if all green; otherwise git merge --abort and reject with the output
git push
git worktree remove /tmp/review-<ID>
git worktree list            # find the worker's worktree for B
git worktree remove --force <worker-worktree-path> && git worktree prune && git branch -D B
```

## 9. Record

- `status: done`, `evidence: "merged <sha>; verify: <n> passed, clippy clean"`.
- Add a LOG.md line and update STATUS.md.
- Promote the report's discovered work into new items, or note why not.

## Rejecting

Set `status: changes` with numbered, concrete `review_notes`: the file and line,
what's wrong, and what passing looks like. "Make it better" is not a note.
