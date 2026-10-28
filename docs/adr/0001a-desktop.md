# ADR 0001a: desktop client stack

- Status: accepted, provisional on one re-run (see "Open gate")
- Date: 2026-10-28
- Plan: `docs/PLAN-v2-ade.md` §4.4; gates: ADR 0001 and `spikes/bench/gates.yaml`
- Evidence: `spikes/bench/results/gpui-kit/desktop_linux/*.json` (M1B-008c)

## Context

ADR 0001 fixed the gates before any prototype and split the client decision in two: desktop
now, from Linux (and macOS) runs that need no devices; mobile in P7 (M1B-011, ADR 0001b).
The desktop candidate in the plan is **gpui-kit** (gpui-component on GPUI). The spike in
`spikes/gpui` renders the common workload: the transcript fed by `agui-fixture`
(`transcript-10k` at 200 events/s), a 5,000-line diff, a terminal pane that keeps receiving
output, and one approval card (M1B-004\*, M1B-008b, M1B-008e).

## Measurements (M1B-008c)

Five runs of 50 s, pinned `gpui-pre@0.3.10` and gpui-component `c8917c36`. The machine is the
cloud dev container: 4 vCPU, **no GPU**, Xvfb, Vulkan through Mesa **lavapipe** (a CPU
rasteriser).

| Gate              | Result     | Desktop gate | Verdict                         |
| ----------------- | ---------- | ------------ | ------------------------------- |
| frame_time_p95    | 56.29 ms   | ≤ 16.7 ms    | miss (CPU rasteriser, see below) |
| key_to_glyph_p95  | 19.15 ms   | ≤ 60 ms      | pass                            |
| cold_start        | 0.154 s    | ≤ 1.5 s      | pass                            |
| rss_peak          | 166.9 MB   | ≤ 300 MB     | pass                            |
| binary_size       | 45.9 MB    | ≤ 80 MB      | pass                            |

## Decision

1. **The desktop client is gpui-kit**, as the plan proposed. Four of the five numeric gates
   pass with wide margins even on a CPU-only rasteriser, and gpui-kit is the only candidate
   that already has the desktop pieces the product needs (editor, virtual lists, docks,
   `alacritty_terminal`) in Rust next to `client-core`.
2. **The frame-time result is not a valid measurement of the gate.** GPUI draws through the GPU;
   lavapipe rasterises every frame on 4 shared vCPUs, so the number measures the container, not
   the stack. ADR 0001 rule 5 (a numeric miss above 25 % counts as a hard failure) is therefore
   not applied to this run. Key-to-glyph, which also depends on frames being presented, passes
   at a third of its budget on the same machine.
3. **Open gate**: frame_time_p95 must be re-measured on a desktop with a GPU (Linux or macOS),
   M1B-008f. If it misses there by more than 25 %, this ADR is superseded and the desktop moves
   to Flutter (the hybrid's mobile stack), reusing `client-core` unchanged.
4. Mobile is not decided here (ADR 0001b, M1B-011, P7, on devices). The default rule of ADR 0001
   (any gpui-kit hard-gate failure on a phone means Flutter on mobile) is unchanged.

## Consequences

- Desktop work (P3 client items, `crates/client-gpui`) proceeds on gpui-kit now; views stay thin
  over `client-core`, so a later switch costs UI code only.
- M1B-008f (human-owned, P7) is the one run that can overturn this decision; until it is
  committed, this ADR stays "provisional".
- Results stay valid only for the pinned revs; a GPUI or gpui-component bump re-runs M1B-008.
