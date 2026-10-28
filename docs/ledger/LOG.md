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
