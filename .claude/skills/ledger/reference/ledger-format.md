# LEDGER.yaml format

```yaml
version: 1
plan: docs/PLAN-v2-ade.md # the plan this ledger executes
branch: v2 # working branch; workers' worktrees start from its HEAD
max_parallel: 2 # workers in flight in the default (rust) lane
lanes: { rust: 2, docs: 1, client: 1 } # per-lane caps on in-flight items
budget:
  max_total_attempts: 120 # per phase window; the leader re-raises it at each phase review
  max_worker_minutes: 45
  max_worker_minutes_client: 60 # timeout for `lane: client` items (GPUI builds are slow)
env: # exported into every worker's environment
  CARGO_TARGET_DIR: /home/user/.cargo-target-shared # shared across worktrees
  CARGO_BUILD_JOBS: '2'
  RUSTFLAGS: '-C link-arg=-fuse-ld=lld'
phase_order: [P0, P1, P2, P3, P4, P5, P6, P7] # primary sort key for picking work
milestone_order: [M0, M1a, M1b, M2, M2b, M3, M4, M5, M5c, M5b, M6] # secondary sort key
items:
  - id: M1A-003 # <milestone>-<seq>[letter], stable forever
    title: ACP client spawns a harness and completes one prompt turn
    milestone: M1a # stable id prefix; must appear in milestone_order
    phase: P1 # required; one of phase_order. Decides when the item is worked
    lane: rust # rust | docs | client — which lane cap applies (default rust)
    priority: 1 # 0 = fix-the-build / tracer, 1 = highest
    size: S # S (<100 lines) | M (<300) | L (split, or Opus)
    risk: low # low | high (high → Opus on attempt 1)
    owner: worker # worker | leader | human
    status: todo # todo | assigned | review | changes | done | blocked
    deps: [M1A-001, M1A-002]
    paths: # the only paths the worker may change (+ implicit, below)
      - crates/sidecar/src/acp/**
    context: # ≥2 entries: plan section + upstream spec / sibling code
      - 'docs/PLAN-v2-ade.md §4.1'
      - https://agentclientprotocol.com/protocol/prompt-turn
    acceptance: # observable pass/fail statements
      - '...'
    verify: # commands the leader re-runs; all must exit 0
      - cargo test -p sidecar --test acp_turn
    min_tests: 3 # optional; for name-filtered cargo test (default 3)
    howto: null # human items only: exactly what to do and where
    # --- maintained by the leader (may be omitted until first touched) ---
    attempt: 0
    assignee: null
    blocked_on: null
    review_notes: []
    evidence: null
```

## Defaults when a field is omitted

| Field                                         | Default  |
| --------------------------------------------- | -------- |
| `lane`                                        | `rust`   |
| `priority`                                    | `2`      |
| `size`                                        | `S`      |
| `risk`                                        | `low`    |
| `owner`                                       | `worker` |
| `attempt`                                     | `0`      |
| `assignee`, `blocked_on`, `evidence`, `howto` | `null`   |
| `review_notes`                                | `[]`     |
| `min_tests`                                   | `3`      |

`phase` has no default: every item must have one.

## Header fields

| Field                              | Meaning                                                                                                                                        |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `max_parallel`                     | Overall cap on workers in flight for the default `rust` lane. Lane caps below can only narrow it for their lane.                               |
| `lanes`                            | Map of lane → max in-flight items. `rust` for code, `docs` for spec/doc-only items, `client` for `crates/client-gpui` and `spikes/gpui` items. |
| `budget.max_total_attempts`        | Stop condition. Set per phase window and re-raised by the leader at each phase review.                                                         |
| `budget.max_worker_minutes`        | Timeout for a worker attempt.                                                                                                                  |
| `budget.max_worker_minutes_client` | Timeout for `lane: client` attempts (defaults to `max_worker_minutes` when omitted).                                                           |
| `env`                              | Variables exported into every worker's shell.                                                                                                  |
| `phase_order`                      | Phases in execution order. Ready items are picked by phase first.                                                                              |
| `milestone_order`                  | Milestones in order, the tie-breaker within a phase (then `priority`).                                                                         |

## Phases and lanes

- **Ordering**: pick ready items by `phase_order`, then `milestone_order`, then `priority`.
  An item from a later phase is picked only when nothing in the current phase is ready, and
  never when it would take a lane slot an earlier phase needs.
- **Deps across phases**: an item must not depend on an item of a later phase, unless the
  dependency is named (by id) in that item's `acceptance` as intentional.
- **P0–P2** items must not depend, even transitively, on any `owner: human` item except the
  ones the plan names for those phases (currently M0-018, the Cloudflare account for P2 deploys).
- **Lanes**: `lane` is optional (default `rust`). Spec/doc-only items use `docs`. Every item
  that touches `crates/client-gpui/**` or `spikes/gpui/**` uses `client`, which is capped at 1,
  so GPUI builds never compete; those items also carry `crates/client-gpui/CONVENTIONS.md` and a
  vendored example under `docs/vendor/gpui-component-<rev>/` in `context`.
- **Review items** (`<milestone>-review`) never appear in another item's `deps`.

## Implicit paths (always allowed, never counted as overlap except Cargo.lock)

- The owning crate's `Cargo.toml`.
- `src/lib.rs` / `src/main.rs` / the parent `mod.rs`, for module declarations and route registration only.
- New files under the crate's `tests/`.
- `Cargo.lock`. Conflicts in it are resolved by the leader.
- `docs/ledger/reports/<ID>.md`.

## Status transitions

```
todo → assigned → review → done
                     ↘ changes → assigned (attempt+1)
any → blocked (blocked_on set) → todo (when unblocked)
human items: blocked(blocked_on: human) → done (leader verifies acceptance)
```

## Rules

- IDs never change and are never reused. To drop an item, set `status: done` with
  `evidence: "dropped: <reason>"`.
- Splitting `X` creates `Xa`, `Xb`, … and drops `X`. Items that depended on `X` now
  depend on the last split part.
- `deps` must reference existing IDs. The leader checks for cycles on every edit.
- `acceptance` entries are pass/fail statements, not tasks.
- `verify` never contains prose. Human confirmation belongs to a `owner: human` item.
