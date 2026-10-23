# LEDGER.yaml format

```yaml
version: 1
plan: docs/PLAN-v2-ade.md # the plan this ledger executes
branch: v2 # working branch; workers' worktrees start from its HEAD
max_parallel: 4
budget:
  max_total_attempts: 250
  max_worker_minutes: 45
env:
  CARGO_TARGET_DIR: /home/user/.cargo-target-shared # shared across worktrees
items:
  - id: M1A-003 # <milestone>-<seq>[letter], stable forever
    title: ACP client spawns a harness and completes one prompt turn
    milestone: M1a
    priority: 1 # 0 = fix-the-build, 1 = highest
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
| `priority`                                    | `2`      |
| `size`                                        | `S`      |
| `risk`                                        | `low`    |
| `owner`                                       | `worker` |
| `attempt`                                     | `0`      |
| `assignee`, `blocked_on`, `evidence`, `howto` | `null`   |
| `review_notes`                                | `[]`     |
| `min_tests`                                   | `3`      |

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
