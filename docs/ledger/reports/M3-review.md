# M3 exit review (desktop app, phase P3)

Reviewer: leader. Date: 2026-11-03. Tree: `v2` after M2B-015 and M3-003a.

## Verify (run on the merged tree)

| Command | Result |
| --- | --- |
| `cargo test --workspace` (Linux, no debuginfo) | 1369 passed, 0 failed, 98 test targets |
| `cargo run -p cli -- e2e app` | PASS in 135.2 s |
| `cargo run -p cli -- e2e tracer` | PASS in 34.4 s |
| CI `rust.yml` on `v2`, linux and tracer jobs | green |
| CI `rust.yml` on `v2`, macos job | red from M3-006 until this review: two tests pressed `ctrl-` keys that are `cmd-` on macOS (paste, new task). Fixed here with `secondary-` keystrokes; see "CI" below for the run that confirms it. |

## Plan §5, P3 "Desktop app": criterion by criterion

| Plan P3 item | Status | Evidence |
| --- | --- | --- |
| Shell (navigation, sign-in, settings, hub) | met | M3-002a/b/c; `tests/shell.rs` (80 tests): pages and shortcuts, pairing sign-in and sign-out, hub field save/refuse, theme |
| Composer | met | M3-004, M3-005, M3-005a/b/c/e, M3-006/6b; `tests/compose.rs`, `tests/compose_commands.rs`, `tests/attachments.rs`: `/` commands, mode/model pickers capped by the runner, queued chips, Stop, refusals, attachments through the session |
| Task view | met | M3-008, M3-008b, M3-009/9a, M3-006a; `tests/transcript.rs`: 10k events virtualized, incremental rows (p95 about 4 ms per frame in debug at 200 ev/s), plan and usage beside the transcript, inline images |
| Approvals | met | M3-011, 11a, 11c, 11g, 11k; `tests/approval.rs` (34): exact ACP options, own option id plus Idempotency-Key, 409 resolver shown, refusal reasons, retry |
| Diff | met | M3-010, 10a, 10d; `tests/diff.rs`: unified/split, chunked, gaps, open file with the workspace root |
| Inbox | met | M3-007, M2-010h/i/k/m/n/o, M3-013d; `tests/inbox.rs` (33): sections, search, filters incl. failed/cancelled, optimistic rename/archive, quick Allow/Deny against the real hub's rows, self-refresh |
| Embedded runner | met, with limits | M2B-015; `tests/local.rs`: in-process runner, per-session and automatic connect-to-phone. Limits filed: M2B-015a-d (thread detach frame, start local session from New task, enrol from the app, phone dispatch to the embedded runner) |
| Palette and shortcuts | met | M3-014; `tests/palette.rs` (27): ⌘K/Ctrl+K, ⌘N, ⌘↵, Alt+Up/Down |
| Notifications | met | M3-013, 13a, 13d; `tests/notify.rs`, `tests/shell.rs`: one notification per transition, focus rules, click-through, hub links, settings |
| Dispatch to a chosen machine (M2b, shipped in P3) | met | M2B-013; live suite `dispatch` (5), `tests/machines.rs` (13) |
| Onboarding and empty states | met | M3-003, 3a; `tests/onboarding.rs` (19) |
| Unsigned macOS builds from CI | met (M3-018) | `desktop.yml` builds `Sprite Machine.app` and uploads `sprite-machine-macos-unsigned` (run 41683096676, 12.8 MB); the rust.yml macos job is green on the same commit |

## Accessibility audit (desktop)

`docs/ux/a11y.md`, named by this item, does not exist in the repo. The audit below uses the plan's cross-cutting list (§3 item 11) instead, and M3-019 adds the document.

| Plan requirement | Desktop status | Evidence |
| --- | --- | --- |
| Screen-reader labels | met in tests | 107 `aria_label` uses and 39 explicit roles (`Status`, `Group`, ...) in `client-gpui/src`; 118 `label()` assertions across 15 test files. CONVENTIONS.md §145: the label doubles as the screen-reader label. |
| Focus order | met in tests | Tab order assertions in `tests/shell.rs` (login), composer and palette focus tests, the regression `shortcuts_keep_working_after_a_page_takes_the_focused_prompt_away` |
| Status announcements | met | banner, waiting banner and media fallbacks use `Role::Status` |
| Reduced motion | not applicable today | no animations in `client-gpui/src` (0 uses) |
| Dynamic Type (text scaling) | **not met** | no text-scale setting or OS scale handling. Filed in M3-019. |
| Strings in Fluent, en only | met | every module has a catalog test that fails on missing and on unused ids |
| Offline queue for messages and approvals | met | client-core outbox (M3-005b/e): persisted, retried with the same Idempotency-Key, bounded |
| Real screen reader (VoiceOver) on a Mac | not checked | needs a Mac (human); added to M3-019's acceptance as a manual check |

## Decisions and known gaps carried forward

- P4 follow-ups filed during M3 stay in the ledger (M3-010e/f, M3-011d/e/h/i, M3-012c, M3-013b/c/e, M3-014a/b, M3-006c/d, M2B-013a, M2B-015a-d, M3-009b/c).
- The shared cargo target dir made workers pick up stale binaries several times; the brief now says to touch sources before every build. Disk filled five times; the env now uses `line-tables-only` debuginfo.

## Verdict

P3 is functionally complete on Linux. Two items stand between this and `v2.0.0-alpha.2`:

1. M3-018: CI builds and uploads an unsigned macOS app.
2. A Mac to dogfood (the plan's P3 human ask), which also covers the VoiceOver check.

The tag is not cut yet; it waits on M3-018 and a green macos job.

## Update 2026-11-03 17:35 UTC

M3-018 and M3-019/19a are done and CI is green on every job (linux, tracer, macos, desktop). P3 is complete except the human items: a Mac to dogfood and the manual VoiceOver pass (M3-019d).
