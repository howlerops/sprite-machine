# Leader review checklist

Run this for every worker result before marking an item `done`.

1. **Scope**: `git diff {branch}...<worker-branch> --stat` touches only the item's `paths`, its report, and tooling-regenerated lockfiles.
2. **Verify**: re-run every `verify` command yourself on the worker branch. Paste the summary into `evidence`.
3. **Acceptance**: check each criterion against the diff and the test output, not the report's claims. Every criterion needs a test or an observable command behind it.
4. **Tests are real**:
   - no `#[ignore]`, `skip`, `todo!()`, commented-out asserts, or loosened lints;
   - new behavior has a test that would fail without the change.
5. **Correctness read**: read the diff adversarially. Look for:
   - error paths that are swallowed;
   - unwraps on network/IO;
   - races in async code;
   - protocol fields mismatched against the spec (A2A / ACP / AG-UI);
   - secrets in code or logs.
6. **Fit**: matches the plan's design, naming and the surrounding code. Nothing gold-plated.
7. **Merge**: merge with a merge commit into `{branch}`. Re-run the workspace-wide checks (`cargo test --workspace`, `cargo clippy --workspace -- -D warnings`) after merging. Push.
8. **Record**: `status: done`, `evidence`, then a LOG.md line. Promote "discovered work" from the report into new ledger items, or note why not.

**Reject** (status `changes`) with numbered, concrete `review_notes`:
- name the file and line, what's wrong, and what passing looks like;
- "make it better" is not a note.
