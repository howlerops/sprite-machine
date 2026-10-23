# LEDGER.yaml format

```yaml
version: 1
plan: docs/PLAN-v2-ade.md      # the plan this ledger executes
branch: v2                     # working branch workers branch from and merge into
max_parallel: 4
items:
  - id: M1A-003                # <milestone>-<seq>, stable forever
    title: ACP client spawns a harness and completes one prompt turn
    milestone: M1a
    priority: 1                # 1 = highest within milestone
    size: S                    # S (<100 lines) | M (<300) | L (split or Opus)
    risk: low                  # low | high (high → Opus worker)
    owner: worker              # worker | leader | human
    status: todo               # todo | assigned | review | changes | done | blocked
    deps: [M1A-001, M1A-002]
    paths:                     # the only paths the worker may change
      - crates/sidecar/src/acp/**
    context:                   # what the worker should read first
      - docs/PLAN-v2-ade.md#41-sidecar
      - https://agentclientprotocol.com/protocol/prompt-turn
    acceptance:                # observable, testable statements
      - "`cargo test -p sidecar acp::turn` spawns the mock ACP agent and receives an end_turn stop reason"
      - "Permission requests are forwarded to a channel, not auto-approved"
    verify:                    # commands the leader re-runs; all must exit 0
      - cargo test -p sidecar acp::
      - cargo clippy -p sidecar -- -D warnings
    # --- maintained by the leader ---
    attempt: 0
    assignee: null
    blocked_on: null           # e.g. "SPRITES_API_TOKEN secret" — set with status: blocked
    review_notes: []
    evidence: null             # "merged abc123; verify: 14 passed"
```

## Status transitions

```
todo → assigned → review → done
                     ↘ changes → assigned (attempt+1)
any → blocked (blocked_on set) → todo (when unblocked)
```

## Rules

- Leader-maintained fields may be omitted until first touched. Defaults:
  `attempt: 0`, `assignee: null`, `blocked_on: null`, `review_notes: []`,
  `evidence: null`, `priority: 2`, `size: S`, `risk: low`, `owner: worker`.
- IDs never change and are never reused. Remove an item by setting `status: done`
  with `evidence: "dropped: <reason>"`.
- `deps` may only point at existing IDs. The leader checks for cycles when editing.
- `paths` of items in the same wave must not overlap.
- `acceptance` items are pass/fail statements, not tasks.
- Human items (`owner: human`) carry a `howto:` field describing exactly what the
  person must do and where (for example: "add SPRITES_API_TOKEN under the environment's Network secrets").
