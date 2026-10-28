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
2026-10-28T04:26Z MVP-000c assigned (haiku)
2026-10-28T04:33Z M1A-007 assigned→done merged 78f3095 (sonnet; hub tracer/state.rs Outcome arms accepted)
2026-10-28T04:33Z M1B-004d merged 5999cf6, status review: release build blocked on disk (<8 GB) — leader-run later
2026-10-28T04:33Z leader: a worker in this batch called the Claude Docs batch tool (not in any brief); no new artifact visible in the artifact list
2026-10-28T04:33Z SPEC-002b done 98ee8f9 (leader): tunnel session_config + client-api §5.5a
2026-10-28T04:33Z M1A-013 re-scoped (pending tasks are hub-owned; session_config command), paths += runtime/handler/client + protocol command; assigned (haiku)
2026-10-28T04:38Z MVP-000c assigned→done merged e3cad67 (haiku; measurement: ACP contributes 0 bytes, name section 34%; one denied send_message call). Added MVP-000f (strip name section). MVP-000e (move validator, gate ACP) not added: no size win
2026-10-28T04:38Z M1A-004a assigned (sonnet; P2 pick — remaining P1 items overlap in-flight bridge files)
2026-10-28T04:47Z M1A-013 assigned→done merged 6f3f33b (haiku; hub sqlite.rs match arm accepted); added M2-008b (hub session route + acp-mock set_mode)
2026-10-28T04:48Z M1A-014 paths += bridge/runtime.rs; assigned (haiku)
2026-10-28T04:48Z M1B-008 lane → docs (scripts only, dry-run verify); assigned (haiku)
2026-10-28T05:18Z M1A-004a assigned→done merged 9b8ca6f (sonnet; cap-std confinement, killpg escalation; worker's client-core clippy failure was shared-target staleness, not real)
2026-10-28T05:18Z leader: shared target clobbered by concurrent worktree builds (hub wasm saw an older libprotocol); leader now verifies in a private target /home/user/.cargo-target-leader
2026-10-28T05:18Z M1A-014 assigned→done merged 59a16f7 (haiku); added M1A-014a (bridge-level no-submit test, image sniffing)
2026-10-28T05:18Z M1B-008 assigned→done merged 13a70f1 (haiku); added M1B-008a/b/c/d (spike instrumentation, workload, real run, xdotool); M1B-009 deps += M1B-008c
2026-10-28T05:18Z M2B-002 merged (sonnet; out-of-path edits accepted: sidecar runtime.rs match arm — superseded by v2's permission.rs on merge — and d1_schema test); leader fixed semantic merge conflict c8d9b4d (SessionConfig arms in machines/core.rs). Added M2-008c, MVP-000d, M2B-002a
2026-10-28T05:18Z M2B-004 paths += runner tracer.rs/lib/main + protocol Sender::from_parts; assigned (sonnet). M2B-011 assigned (haiku). M1B-008a assigned (haiku)
2026-10-28T05:37Z M2B-002 review→done (workspace tests, live machine_do/thread_do/authz, tracer green on merged tree)
2026-10-28T05:46Z M2B-011 assigned→done merged 8c932b6 (haiku; one denied stray interrupt_session call); added M2B-011a (refname encoding + CAS)
2026-10-28T05:46Z M1A-014a paths += runtime.rs (test FilePolicy injection only); assigned (haiku)
2026-10-28T06:02Z leader: disk hit 100% (shared 13G + leader private target 9.5G); removed the leader target. Leader private target is wiped after each verify from now on
2026-10-28T06:02Z M1B-008a assigned→done merged 3e50593 (haiku; tracer not re-run: spikes/ is outside the workspace). Notes for M1B-008c: frame t_us is post-present hook time, marker = shell+focused composer (not first rows)
2026-10-28T06:03Z M1B-008d assigned (haiku)
2026-10-28T06:06Z M1A-014a assigned→done merged d144a60 (haiku; no runtime.rs change needed)
2026-10-28T06:06Z M1B-008d assigned→done merged 071a6ef (haiku)
2026-10-28T06:09Z M2B-004 assigned→done merged 51725de (sonnet; Cargo.toml dep union conflict with M2B-011 resolved by leader); added M2B-004a (durability, high), M2B-004b (wss), M2B-004c (presence/thread_open)
2026-10-28T06:21Z M1B-004d review→done (leader release build: 41.0 MB stripped, under the 80 MB desktop gate; separate release target deleted after)
2026-10-28T06:22Z M1B-008b phase P1→P2 (benchmark workload feeds M1B-008c/M1B-009, both P2; not part of the P1 Engine deliverable)
2026-10-28T06:22Z leader: CI jobs fail in ~3 s with no logs since run 51 (04:47Z), no workflow change → added CI-001 (human: Actions billing/limit). Local verification stands in until it is fixed
2026-10-28T06:24Z M2-014 blocked (human): permission check refused creating sm-hub D1/R2/queue in the shared Cloudflare account; commands in docs/ops/hub.md; secrets split to M2-014a (human, after M2-015). M0-009 blocked (human) for the same reason (preview deploy)
2026-10-28T07:13Z P1 phase review written (docs/ledger/reviews/P1.md): clean rebuild, 506 tests 0 failed, live hub tests + tracer green; CI not evidence (CI-001). Continuing into P2 per the user's go
2026-10-28T07:13Z M2B-004a assigned (sonnet, high), M1A-006c assigned (sonnet, high), M2-008c assigned (haiku; paths += machines/internal.rs event_id pass-through)
2026-10-28T07:29Z M2B-004a assigned→done merged 906be80 (sonnet). Leader review: bridge on_incoming still commits on receipt, so the end-to-end crash window remains with a shared Store → added M1A-006d (deps M1A-006c, which owns bridge files now)
2026-10-28T07:30Z M2B-004b assigned (haiku)
2026-10-28T07:36Z M1A-006c assigned→done merged cbc26b7 (sonnet); added SPEC-004 (leader: MCP servers source, harness-gone outcome) and M1A-006e (spawn_with in runner, unknown_interrupt mapping, acp-mock recording)
2026-10-28T07:37Z M1A-006d assigned (sonnet, high)
2026-10-28T07:37Z SPEC-004 done 948fc84 (leader): harness exit → cancelled + harness_exited; MCP servers from runner config until loadouts. M1A-006e acceptance updated to follow it
2026-10-28T07:41Z M2B-004b assigned→done merged adcf82c (haiku; one read-only read_documentation call, harmless). Private-CA roots + TLS trust in TunnelConfig noted for the runner config item, not added separately
2026-10-28T07:41Z M2B-011a assigned (haiku)
2026-10-28T07:47Z M2B-011a assigned→done merged eba1bf6 (haiku; one denied stray create_session call); leader fix 5a89c03 for its discovered item (ids validated in every entry point)
2026-10-28T07:50Z M1A-006d assigned→done merged e747f34 (sonnet). End-to-end crash window closed
2026-10-28T07:50Z M1A-006e acceptance += prune applied-keys set; assigned (haiku). M2B-004c assigned (haiku)
2026-10-28T07:51Z leader: disk 3.6 GB free → deleted 52 stale test executables (1.9 GB) + stale workspace rlibs older than 60 min from the shared target; 5.4 GB free
2026-10-28T07:56Z M2B-004c assigned→done merged 8df4417 (haiku); added M2B-004d (sleep hook, thread_open once). Call sites of thread_open belong to the local-session items (M2B-007 sm rc / TUI)
2026-10-28T07:56Z M2-009 assigned (haiku)
2026-10-28T08:06Z M2-008c merged (haiku); criterion 1 Machine-DO half done by leader bc9ef61 (ThreadHost::append carries event_id; test mutation-checked); verify running. Added M2-008d (high: POST /v1/threads accepts a foreign/unknown machine_id → org check) and CI-002 (per-worktree hub wasm target)
2026-10-28T08:06Z leader: SPEC-004 amended 65e63e4 (harness exit uses the resume run like cancel/expiry). Added M1A-006f (tracer spawn_with, mapper N interrupts, remove fail_stale_interrupts)
2026-10-28T08:06Z leader: disk — deleted a worker's private target in the scratchpad (4.3 GB) and the stale /root/.cargo-target-shared (3.9 GB); 14 GB free
2026-10-28T08:14Z M2-008c → done (verify green). M2-008d assigned (sonnet, high)
2026-10-28T08:15Z M2-009 merged (haiku) + leader e676b37: removed the in-source dev signing key fallback; hub-test.sh passes a random per-run ATTACHMENT_SIGNING_KEY. Added M2-009a (D1 idempotency, sweep). Verify running
2026-10-28T08:15Z M1A-006e merged (haiku); criterion 1 (tracer spawn_with) blocked by paths → M1A-006f
2026-10-28T08:26Z M2-009 → done, M1A-006e → done (verify: 379 passed, live attachments/thread_do/authz, tracer). M2-008d merged 78f8606 (sonnet), verify running
2026-10-28T08:26Z MVP-000d paths widened (sidecar tracer, hub machines HostKind::Tracer, wrangler DO deletion migration, tests); assigned (sonnet). M2-008b paths += sidecar session_config test; assigned (haiku). M2B-004d assigned (haiku). M1A-006f deps += MVP-000d
2026-10-28T08:47Z M2-008d → done (live: foreign/unknown/revoked machine all 404 with nothing written). Tracer exited 1 once with no output under load; PASS on re-run
2026-10-28T08:47Z M2B-004d assigned→done merged 17a420c (haiku); added M2B-004e (startup wiring, logind inhibitor, macOS watcher)
2026-10-28T08:48Z M2B-005 assigned (sonnet; size L)
2026-10-28T08:51Z M2-008b merged 115136a (haiku). Leader review: its session handler gated only on the caller's own role. Fixed 609d261: policy rule (member, thread.session) + route through forward/authorize_thread
2026-10-28T08:51Z SECURITY (leader review): authorize_thread never checks that a thread belongs to the caller's org; with a valid session, any member can subscribe/prompt/answer/cancel on another org's thread if they know its id. Not deployed, so no exposure. Added M2-008e (priority 1, high, after MVP-000d)
2026-10-28T08:52Z MVP-000d back (sonnet, all verify green per worker); added M1A-006g (explicit local-prompt flag instead of the "local:" key prefix; stale tracer wording). Merge after the M2-008b verify finishes
2026-10-28T08:59Z M2-008b → done (verify: 323 passed, thread_session/authz/thread_do live, tracer). MVP-000d merged bd4e526 (sonnet), full verify running
2026-10-28T09:20Z MVP-000d → done (clean verify: 590 passed, 7 live hub suites, tracer on /v1). Disk: stale test executables cleared, 11 GB free
2026-10-28T09:20Z M2-008e assigned (sonnet, priority 1 security). M1A-006f assigned (haiku)
2026-10-28T09:25Z M2B-005 merged 0ed5a67 (sonnet; out-of-path db/mod.rs MIGRATIONS entry accepted). Review: device-code remote-phishing risk (attacker approves a guessed code into their org) → M2B-005a (priority 1: tunnel trust gate, approve rate limit, runner confirms approving org). M2B-005b (logout/status/run). Verify running
2026-10-28T09:34Z M1A-006f back (haiku): criteria 2, 3 met (mapper resolves requests buffered during a pause; fail_stale_interrupts removed); criterion 1 (tracer spawn_with) blocked again by a test file outside paths (leader scoping) → folded into M2B-007 acceptance (sm rc builds bridges with spawn_with; KeychainCredentials; sleep watcher; thread_open). SPEC-004 amended 1e1126d (one resume run per buffered request)
2026-10-28T09:38Z M2B-005 → done (verify: hub+runner 251, devices/machine_do/authz live, tracer, no openssl). M1A-006f merged 80b58f8 → done (293 passed, tracer)
2026-10-28T09:38Z M2B-006 assigned (haiku), M1B-008b assigned (haiku). WEB-001 held until M2-008e lands (hub auth files)
2026-10-28T09:44Z M2B-006 assigned→done merged b316bb5 (haiku); added M2B-006a (confirm invocations), M2B-006c (MCP command resolution, sse, ${VAR}); M2B-007 acceptance += discovery/MCP wiring. Harness flagged the report as settings-json-shaped: it describes reading ~/.claude.json for MCP servers (the feature); no instructions
2026-10-28T09:44Z M2B-004e assigned (haiku)
2026-10-28T09:57Z M2B-004e assigned→done (haiku); added M2B-004f (macOS build + real sleep test, mock logind; deps CI-001)
2026-10-28T09:57Z M1B-008b merged 397fc7b (haiku); worker's final verify hit the full disk — leader re-running spike tests/clippy on the merged tree
2026-10-28T09:57Z leader: disk full again — M2-008e worker built into a private worktree target/ (4.7 GB) against the brief; asked it to switch to the shared target and delete it
2026-10-28T09:58Z M1B-008b → done (leader verify: spike 86 passed, clippy). M1B-008c acceptance += workload var, continuous terminal output, frame coverage. Disk 1.5 GB → deleted the M2-008e worker's private target (5.5 GB); 7.4 GB free
2026-10-28T09:59Z M2B-007 assigned (sonnet; paths += main.rs). M2B-006c assigned (haiku). Briefs now say: never build into a worktree target/
2026-10-28T10:04Z M2B-006c assigned→done merged 7d65ece (haiku); signature change announced to the M2B-007 worker. Discovered M2B-006f/g (warning assertions, project-relative commands) noted, not filed (low value)
2026-10-28T10:05Z M2B-006a blocked (human): the permission check refused installing/running third-party agent CLIs (Code from External); howto asks the user to run the probe or allow it
2026-10-28T10:05Z MVP-000f re-scoped (Rust strip step in cli, no binaryen download; 25% vs the current bundle) and assigned (haiku)
2026-10-28T10:06Z M2-008e merged 6d3f90f (sonnet; leader resolved MIGRATIONS conflict with M2B-005: both 0005 and 0006). Leader read the org check (one lookup, same 404 for missing/foreign, before audit). Full verify running. Added M2-008f (P4)
2026-10-28T10:23Z M2B-007 back (sonnet). Review: --permission-mode applied after the first prompt starts (a safer mode like plan is not in force for prompt 1) → M2B-007a (priority 1, high) with mcp_capabilities exposure, worktree sweep, session end. Merge after the M2-008e verify
2026-10-28T10:30Z M2-008e → done (full verify: 649 passed; 8 live suites, thread_create on re-run; tracer)
2026-10-28T10:30Z M2B-007 merged 7e31975 (sonnet), MVP-000f merged 2868c5c (haiku; leader re-scoped criterion 2: 25% gzip unreachable by stripping names, -13% gzip/-35% raw accepted). Added MVP-000h (P4). Verify running
2026-10-28T10:30Z M2B-007a assigned (sonnet, priority 1), M2B-005a assigned (sonnet, priority 1 security; paths += migration 0007, cron, DELETE /v1/devices/self)
2026-10-28T10:37Z M2B-007 → done, MVP-000f → done (verify8: runner+cli 137, hub-test thread_do/machine_do, tracer)
2026-10-28T10:37Z M2-010 assigned (haiku); aligned to client-api §5.2 (GET /v1/threads, not v1's /api/tasks), org-scoped list; paths += threads sync hook, policy
2026-10-28T10:38Z SPEC-005 done 1c2c2b0 (leader): browser sign-in (runner QR link + browser device code). Added M2-003c (priority 1, hub routes + sm rc QR); WEB-001 deps += M2-003c, pairing criterion follows §5.7
2026-10-28T10:50Z M2B-007a assigned→done merged (sonnet; one-line out-of-path main.rs change accepted); added M2B-007b
2026-10-28T10:50Z M2-011 assigned (sonnet; crypto: RFC 8291/8292 with RFC test vectors)
2026-10-28T10:56Z M2B-005a merged d99e1af (sonnet; out-of-path main.rs --yes plumbing accepted, auto-merged with M2B-007a). Full verify running. Added M2B-005c (close live untrusted tunnels on policy flip; token per-IP limit)
