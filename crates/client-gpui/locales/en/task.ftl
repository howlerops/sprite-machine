# Plan view and usage meter strings (en). Referenced from src/task/plan.rs and src/task/usage.rs,
# checked by tests/plan_usage.rs.

plan-title = Plan
plan-empty = No plan yet
plan-summary = { $done } of { $total } done
plan-region-a11y = Plan: { $done } of { $total } done
plan-status-pending = Pending
plan-status-in-progress = In progress
plan-status-completed = Completed
plan-priority-high = High priority
plan-priority-medium = Medium priority
plan-priority-low = Low priority
plan-row-a11y = { $content }: { $status }, { $priority }

usage-title = Usage
usage-region-a11y = Usage
usage-turn-tokens = This turn: { $tokens } tokens
usage-total-tokens = Total: { $tokens } tokens
usage-turn-cost = This turn: { $cost }
usage-total-cost = Total: { $cost }
usage-cost-value = { $amount } { $currency }
usage-cost-unknown = Cost unknown
usage-tokens-k = { $value }k
usage-tokens-m = { $value }M
usage-context = Context: { $percent }% of { $size } used
usage-context-unknown = Context window unknown
usage-context-a11y = Context { $percent } percent used
