# Ledger log

2026-10-23T22:47Z ledger rewritten from 4-lens review (135 items)
2026-10-24T03:08Z added M5c flows (18 items)
2026-10-27T14:36Z added M2b Remote Control (21 items)
2026-10-27T16:51Z ledger rev 5: hub on Cloudflare, TurnEvent inside / A2A+AG-UI at edges, mobile web page, phases P0–P7 on every item, lanes; 174 → 186 items (+SPEC-001/002, MVP-000, GPUI-000, M0-017/018/019, M2-016, WEB-001..004)
2026-10-27T17:09Z leader: scripts/cloud-setup.sh tested (3.5 min, lavapipe OK); docs/setup/SETUP.md added
2026-10-27T20:19Z ledger rev 6: all Cloudflare (Sprites → Cloudflare Sandboxes via a TS Workspace DO); dropped M0-001/012/016, added M0-020 (model key in Secrets Store), retargeted M0-008a/b/c, sandbox spikes M0-009/010/011/013/015, M1A-010/011/012/016/017, M2B-003/016, M4-_, M5-_/M5C-008, M6-002/003; 186 → 187 items (184 open)
2026-10-27T22:11Z leader: reconcile — no orphaned items; M0-018 blocked→done (Cloudflare secrets present, wrangler whoami + containers list OK)
2026-10-27T22:11Z M0-002 todo→done merged 738d66e (17 crates, rust.yml with macos-latest)
2026-10-27T22:18Z SPEC-001 todo→done merged ba99e20 (27 variants, 32 examples)
2026-10-27T22:18Z SPEC-002 todo→done merged 24908dd
2026-10-27T22:18Z M0-003 todo→assigned attempt 1 (haiku, client lane)
2026-10-27T22:18Z M0-006 todo→assigned attempt 1 (haiku, rust lane)
2026-10-27T22:20Z M1B-003 todo→done merged 84a8c7e
2026-10-27T22:20Z M0-019 blocked→done (macos-latest job green on v2, run 40593841232)
2026-10-27T22:32Z M0-006 assigned→done merged cb36372 (discovered work: SDK dep for the runner folds into M1A-003; schema-bump note kept in report; tokio/wasm guard already in rust.yml; no new items)
2026-10-27T22:32Z M1A-002 todo→assigned attempt 1 (haiku, rust lane)
2026-10-27T22:34Z M0-003 assigned→done merged b9b0bbf (discovered: clean-image test skipped, cloud-setup.sh already covers it; gpui cache-miss checked in GPUI-000)
2026-10-27T22:34Z M0-017 todo→assigned attempt 1 (opus, risk high)
2026-10-27T22:34Z GPUI-000 todo→assigned (leader, client lane)
2026-10-27T22:53Z M0-017 assigned→done merged 7e1a66a (opus); leader decision: 'no tokio' → 'no tokio runtime' (worker 0.8.7 depends on tokio with no features); plan §4.2, M0-002/M0-017/M2-* criteria and rust.yml now use scripts/check-no-tokio-runtime.sh. Discovered: hibernation externs + deployed hibernation check noted for M2-008/M2B-002 briefs, not new items
2026-10-27T22:54Z M2B-001 todo→assigned attempt 1 (haiku; P1 pick — no P0 rust item ready, slot kept free for MVP-000)
2026-10-27T23:06Z leader: container restart (OOM from parallel builds) — reconciled, nothing lost; M1A-002 + M2B-001 review passed, merge queued behind GPUI-000; added M2B-001a (strict frames); M2B-004 context += M2B-001 report
2026-10-27T23:13Z GPUI-000 assigned→done merged d254903 (gpui-pre =0.3.10 + gpui-kit/gpui-component @c8917c3; vendored examples+stories+skill; CONVENTIONS.md; CI apt deps)
2026-10-27T23:13Z M1A-002 review→done merged 1758934 (discovered: session/load replay + fs steps + ProtocolVersion re-export deferred to the runner items that need them; strict ACP enum parsing noted)
2026-10-27T23:13Z M2B-001 review→done merged 8d32c8b
2026-10-27T23:13Z MVP-000 todo→assigned attempt 1 (opus, priority 0)
2026-10-27T23:13Z M1A-001 todo→assigned attempt 1 (haiku; P1, no path overlap with MVP-000)
2026-10-27T23:13Z leader: could not delete stale branch leader/GPUI-000-wip (push --delete → HTTP 403)
2026-10-27T23:21Z M1A-001 assigned→done merged 48a29b6 (discovered: SM_GIT_SHA build script → release items; hub URL validation → M2B-004; no new items)
2026-10-27T23:22Z M1A-003 todo→assigned attempt 1 (haiku; no overlap with MVP-000)
2026-10-27T23:48Z leader: added MVP-000a, MVP-000b, SPEC-003 from MVP-000 report; M2-008 + M3-001 now depend on SPEC-003; M1A-003 review passed (ignored re-exec helper accepted as logged exception → M1A-003b)
2026-10-28T00:04Z MVP-000 review→done merged e60a768 (leader resolved sidecar Cargo.toml dependency union with M1A-001)
2026-10-28T00:04Z M1A-003 review→done merged 4896911 (removed duplicate protocol dep after auto-merge)
2026-10-28T00:04Z SPEC-003 todo→done merged 2e60d44; tracer's runner-emitted UserMessage is a stub deviation, fixed by M2-008/M1A-006b
2026-10-28T00:04Z leader: tracer green after merges (110 tests)
2026-10-28T00:05Z M2-001 paths += crates/hub/src/tracer/worker.rs (must move the tracer's #[event(fetch)] into the real router)
2026-10-28T00:05Z M1B-002 todo→assigned attempt 1 (haiku, last P0 worker item)
2026-10-28T00:05Z M2-001 todo→assigned attempt 1 (haiku)
2026-10-28T00:17Z leader: disk full (session allowance) stopped M2-001 + M1B-002 mid-attempt; freed ~8 GB (incremental cache, spike/review/scratch targets, gpui clone); ledger env CARGO_INCREMENTAL=0; workers resumed one at a time on the same attempt
2026-10-28T00:28Z M2-001 assigned→done merged bad9ea5 (worker hit disk-full mid-attempt, resumed same attempt; discovered: JSON 405 noted for M2-008; hub size profile = MVP-000a)
2026-10-28T00:28Z MVP-000b todo→assigned attempt 1 (haiku, CI only)
2026-10-28T00:28Z M1B-002 resumed (same attempt) after M2-001
2026-10-28T00:29Z MVP-000b assigned→review merged 55a802f; done when the CI tracer job is green
2026-10-28T00:45Z M1B-002 review→done merged b2948e1 (resumed after disk incident, same attempt); discovered: validate_sequence folds into M1A-006a; terminal closing snapshot clarified in turn-event.md §4.4
2026-10-28T00:45Z leader: post-merge tracer failed once on a stale cli binary (compile-time CARGO_MANIFEST_DIR from a deleted review worktree via shared target); rebuilt → PASS; runtime hub lookup added to MVP-000a acceptance
2026-10-28T00:45Z M1B-004a todo→assigned attempt 1 (opus: first GPUI screen, plan §7.4)
2026-10-28T00:45Z MVP-000a todo→assigned attempt 1 (haiku)
2026-10-28T00:57Z MVP-000a assigned→done merged f75bb81 (wasm-release profile; 507→412 KB gzip; runtime hub lookup)
2026-10-28T00:57Z leader: disk at 2.4 GB during M1B-004a build; removed stale release/ and wasm32 release/ outputs (→2.9 GB); full shared-target rebuild planned at P0 review
2026-10-28T01:02Z MVP-000b review→done (CI tracer job green, run 40609114253)
2026-10-28T01:02Z leader: macos CI red since M1A-003 merge (/proc-only test); fixed in 5b435f3 (kill -0, mutation-checked)
2026-10-28T01:04Z M1B-004a assigned→done merged 0007480 (opus); CONVENTIONS.md updated with its GPUI API notes; discovered work → M1B-004b..d / M1B-008 (variable-height rows, streaming + frame timing)
2026-10-28T01:04Z leader: all 12 P0 items done — P0 phase review starts (clean rebuild of the shared target)
2026-10-28T01:30Z leader: P0 phase review complete (docs/ledger/reviews/P0.md): clean rebuild, 129 tests, tracer, hub smoke, gpui spike, schemas all green; CI run 40612758905 green; no tag (plan §5 tags alpha.1 at P2)
2026-10-28T02:03Z user: go-ahead to drive the ledger through all phases; worker models = haiku + sonnet only (no opus workers). SKILL.md, worker-brief.md and the ledger header updated; new lane ts:1 (M0-008a)
2026-10-28T02:03Z P1 batch 1: M1A-006a (sonnet), M3-001 (haiku), M0-008a (sonnet, ts lane) assigned
2026-10-28T02:11Z M0-008a assigned→done merged 9e300d4 (sonnet; SDK 1.0.3 has no getSandbox/exposePort — uses ctx.container + DirectoryBackup; wrangler.dev.jsonc workaround until M0-008c image exists)
2026-10-28T02:11Z M1B-004b todo→assigned attempt 1 (haiku, client lane)
2026-10-28T02:29Z M1B-004b assigned→done merged e3fc24c (haiku); CONVENTIONS.md += Diff component + test-module gotcha
2026-10-28T02:29Z M1B-004c todo→assigned attempt 1 (haiku, client lane)
2026-10-28T02:31Z M3-001 assigned→done merged 5a7418f (haiku); added M3-001a; M2-008 context += client-api alignment (spec issues 3-4)
2026-10-28T02:31Z M2-008 paths += tracer/{thread,state,mod}.rs + router.rs (replaces the tracer thread stub; route registration); assigned attempt 1 (sonnet, risk high)
2026-10-28T02:34Z M1A-006a assigned→done merged a41219c (sonnet); post-merge: 233 workspace tests, tracer PASS; added SPEC-001a (leader) for the mapper's spec issues
2026-10-28T02:35Z M2-016 todo→assigned attempt 1 (haiku); M2-008 spawned from v2 @307667b (has the sequence validator)
2026-10-28T02:35Z SPEC-001a todo→done merged 1ecc2b9
2026-10-28T02:52Z M1B-004c assigned→done merged bf85e18 (haiku); CONVENTIONS.md += keyboard test helpers, Rust 2024 capture note
2026-10-28T02:52Z leader: M1B-004d (release gpui build ~5 GB) held until disk is cleaned (6.8 GB free)
2026-10-28T02:55Z M2-016 assigned→done merged bfc51e3 (haiku); discovered: D1 runtime route + retention/audit-export jobs noted for later M2 items; agents↔machines FK → M2B-002
2026-10-28T02:55Z M0-008b todo→assigned attempt 1 (haiku)
2026-10-28T02:55Z leader: briefs now forbid claude-code-remote / session / messaging tools (workers called list_sessions/send_message)
2026-10-28T03:07Z M0-008b assigned→done merged acea2f8 (haiku; one denied archive_session call, commit msg mixup self-fixed)
2026-10-28T03:07Z leader: rust lane cap 2→3 (non-GPUI builds are light; disk watched)
2026-10-28T03:07Z M1A-009 (sonnet) + M1A-004 (haiku) assigned attempt 1
2026-10-28T03:12Z M2-008 assigned→done merged 123355e (sonnet); worker wrote a private <worktree>/target (4.5 GB) — deleted; added M2-008a (redaction, catch-up cap, compaction, DEV_FIXTURES); M2B-002 context += Thread DO hooks
2026-10-28T03:12Z M1A-009 re-dispatched (same attempt; first dispatch stopped on disk)
2026-10-28T03:13Z M2-003a paths += threads/auth.rs (replace the placeholder), auth/** , migrations/*auth*; risk→high (security); assigned attempt 1 (sonnet)
2026-10-28T03:28Z M1A-004 assigned→done merged 50d7c45 (haiku)
2026-10-28T03:28Z M1A-009 assigned→done merged 18aaec8 (sonnet; additive client.rs resume/load methods accepted; leader resolved acp/mod.rs)
2026-10-28T03:28Z added M1A-004a (terminal/fs hardening, sonnet); M2B-004 + M1A-006b context += Store/Workspace/Terminals APIs
2026-10-28T03:37Z M2-003a merged 827a41c (sonnet; out-of-path edits db/mod.rs MIGRATIONS, threads/routes.rs gate, hub-test.sh random bootstrap var — accepted); leader wiped the shared target (disk 3.8→23 GB) and runs a clean-rebuild verify
2026-10-28T03:37Z M1A-006b paths += sidecar/src/tracer*.rs (it replaces those stubs); assigned (sonnet); M2B-001a assigned (haiku)
2026-10-28T04:07Z M1A-006b assigned→done merged 11112cd (sonnet); added M1A-006c (bridge robustness, P2) and SPEC-002a (done by leader: approval outcome union)
2026-10-28T04:07Z M2B-002 paths += tracer/**, threads/** (push hook), worker.rs, db/mod.rs, protocol tunnel Outcome, runner tracer (tunnel client adapter); deps += SPEC-002a; assigned (sonnet)
2026-10-28T04:07Z M1B-004d (haiku, client) + M3-001a (haiku) assigned
2026-10-28T04:12Z M3-001a assigned→done merged a09751c (haiku; one denied create_session call); added M3-001b (P3)
2026-10-28T04:13Z M1A-006c: elicitation moved to M1A-007 (dedupe); deps += M1A-007
2026-10-28T04:13Z M1A-007 paths += bridge/handler.rs, runtime.rs, protocol tunnel types (Outcome union; M2B-002 may land it first); assigned (sonnet)
2026-10-28T04:13Z M2B-003 assigned (haiku)
2026-10-28T04:25Z M2B-003 assigned→done merged 99b65cc (haiku; one denied archive_session call)
2026-10-28T04:25Z leader: shared CARGO_TARGET_DIR gives workspace members the same metadata in every worktree, so parallel worktrees overwrite each other's member artifacts (seen: sidecar compiled against another branch's protocol). Verifies now touch crates/*/src/{lib,main}.rs first; briefs say the same
