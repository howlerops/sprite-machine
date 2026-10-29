# Hub: alerts

Cloudflare Notifications send the alerts. The signals come from Workers Metrics and Observability
(Workers Logs), Queues and D1 metrics, and Analytics Engine for custom counters. The rules are
set once in the dashboard; this file is the list and the thresholds to start from. Workers:
`sm-hub-production`, `sm-workspace`.

## Rules

| Alert | Signal | Start threshold | Severity |
| ----- | ------ | --------------- | -------- |
| Error rate | Workers Metrics: errors / requests for `sm-hub-production` (5xx and uncaught exceptions) | over 2% for 5 minutes | page |
| Health | `GET $HUB_URL/healthz` not 200 (Health Check, or any external probe) | 2 failures in a row | page |
| CPU time | Workers Metrics: CPU time p99 | over 60% of the plan limit for 15 minutes | warn |
| Durable Object errors | Workers Metrics for the DO classes (Thread, Machine, Flow, WorkspaceDO); "Durable Object" exceptions in Workers Logs | any errors sustained 5 minutes | page |
| Queue backlog | Queues `sm-hub-notify`: backlog messages, oldest message age | backlog over 500, or oldest over 5 minutes, for 10 minutes | warn |
| Queue failures | Messages that exhaust `max_retries = 5` | any | warn |
| Cron failures | Workers Logs: the hourly `17 * * * *` scheduled event ends in an exception | one failed run | warn |
| D1 | D1 metrics: query errors; storage near the per-database limit | errors for 5 minutes; storage over 70% | warn |
| Deploy failed | GitHub Actions `deploy-hub` failed | any failure | warn |

## Setting them in the Cloudflare dashboard

1. Destinations: Manage Account, Notifications, Destinations. Add an email, a webhook (Slack or
   PagerDuty) and optionally mobile push. Send a test to each.
2. Worker rules: Workers & Pages, `sm-hub-production`, Observability, Alerts (or Notifications,
   Add). Pick the metric (errors / requests, CPU time p99, DO errors), the threshold and window from
   the table, and the destination.
3. Queue rules: Queues, `sm-hub-notify`, Metrics shows backlog and age. Add the alert in
   Notifications, Add, if the Queues product is offered on the plan; otherwise use an Observability
   query on the consumer.
4. Health: Notifications, Add, "Health Check Status" (needs a Health Check on a zone with a
   custom domain), or an external uptime probe on `/healthz`.
5. Platform incidents: Notifications, Add, "Cloudflare Status" for Workers, D1, R2, Queues and
   Durable Objects.
6. Put page-level rules under one policy name (`sm-hub page`) and warn-level under another
   (`sm-hub warn`) so changing a destination is one edit.
7. Test: set one warn threshold to 0 for a minute, confirm the message arrives, set it back.

The API (`POST /accounts/{account_id}/alerting/v3/policies`) can create the same policies. It needs a
token with Notifications edit permission, which the deploy token must not have.

## Investigating

- Workers Logs: Workers & Pages, `sm-hub-production`, Logs; filter on outcome not `ok`. Live:
  `wrangler tail --env production --status error`.
- Cron: Settings, Triggers, Cron Triggers.
- Queue: `wrangler queues info sm-hub-notify`. D1: `wrangler d1 info sm-hub`.

## Custom metrics (Analytics Engine)

For rates the platform does not show (push delivery failures, auth failures, rate-limit hits, per-org
usage), the hub can write data points to an Analytics Engine dataset: binding `METRICS` in
`[[env.production.analytics_engine_datasets]]`, dataset `sm_hub`. This is not wired yet (discovered
work in the M2-015 report). Once it is, query with the SQL API:

```
SELECT toStartOfInterval(timestamp, INTERVAL '5' MINUTE) AS t, blob1 AS event, sum(_sample_interval * double1) AS n
FROM sm_hub WHERE timestamp > NOW() - INTERVAL '1' HOUR GROUP BY t, event ORDER BY t
```

Analytics Engine has no alert rules of its own: poll that query from a scheduled job and send to the
same destinations, or chart it in Grafana with a threshold alert.
