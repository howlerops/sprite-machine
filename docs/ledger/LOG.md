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
