# Runbook: GitHub outage or rate limiting

## Symptoms

- `WebhookAckSlow` (p99 webhook acknowledgement > 1 s): our side is slow; GitHub retries and
  can disable a slow hook.
- `GitHubRateLimitLow` (an installation under 10% of its API quota).
- Tips stuck in `verifying`; replies missing; githubstatus.com shows an incident.

## Check

1. `webhook_ack_seconds` p99 and api latency; database latency (the webhook only stores and
   queues, so slowness is usually the database).
2. `github_ratelimit_remaining` by installation: one noisy installation, or all?
3. Dead jobs of kind `verify_tip`, `post_reply`, `resolve_held`.

## Steps

1. GitHub down: nothing to do. Verification never treats an outage as "didn't contribute"; jobs
   retry, and rate-limited jobs wait until GitHub's reset time. Webhooks GitHub couldn't
   deliver can be redelivered from the App's settings (Advanced → Recent deliveries); duplicates
   are harmless (`delivery_id` is unique, tips are idempotent per comment).
2. Slow acks: scale the api (`fly scale count 3 -a mergetip-api-production`) and check the
   database.
3. One installation exhausting its quota: look for a spam pattern
   (`SELECT sender_id, count(*) FROM tips WHERE created_at > now() - interval '1 hour' GROUP BY 1 ORDER BY 2 DESC LIMIT 5;`);
   per-sender limits apply (20/hour), but a suspended installation can be cut off with
   `UPDATE installations SET suspended_at = now() WHERE github_installation_id = <id>;`.

## Verify recovery

`webhook_ack_seconds` p99 < 1 s, `jobs_ready` back near 0, `verifying` tips moving.
