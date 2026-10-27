# Runbook: payments failing or slow

## Symptoms

- `TipSuccessRateLow` (paid ÷ (paid + failed) < 95% over 1 h), `TipLatencyHigh` (p50
  comment-to-receipt > 10 s), `JobQueueStalled` (oldest ready job > 5 min), or
  `EmailBounceRateHigh`.
- Bot replies "😕 … couldn't be paid" in many threads.

## Check

1. Which error codes: `SELECT error_code, count(*) FROM tips WHERE state = 'failed' AND updated_at > now() - interval '1 hour' GROUP BY 1 ORDER BY 2 DESC;`
   - `quota_exceeded`, `insufficient_balance`, `daily_cap_reached`: sender-side; nothing to fix
     unless one sender dominates (abuse).
   - `receiver_unpayable`: one LNURL provider down? Group by receiver domain.
   - `payment_failed`, `wallet_invalid`: wallet or relay side → [relay-outage.md](relay-outage.md).
2. `nwc_errors_total` by `code`: a spike in `TIMEOUT` or `RELAY` means relays
   ([relay-outage.md](relay-outage.md)).
3. Queue: `jobs_ready`, `jobs_oldest_age_seconds`, `jobs_dead`, then
   `SELECT kind, count(*), max(last_error) FROM jobs WHERE dead_at IS NOT NULL GROUP BY 1;`
4. Worker health: `fly status -a mergetip-worker-production`, logs for `job failed`.

## Steps

1. Worker down or crash-looping: `fly logs -a mergetip-worker-production`, roll back with
   `fly deploy --image <previous image>` if a release caused it.
2. Queue stuck behind one kind: dead jobs keep their payload; after fixing the cause, re-queue
   them: `UPDATE jobs SET dead_at = NULL, attempts = 0, run_at = now() WHERE dead_at IS NOT NULL AND kind = '<kind>';`
   (`pay_tip` jobs are safe to re-run: an open attempt is reconciled, never paid twice.)
3. If payments might be wrong rather than just failing, stop them first:
   [disable-payments.md](disable-payments.md).

## Verify recovery

`tips_final_total{state="paid"}` rate back to normal, `jobs_oldest_age_seconds` < 60,
alert resolved.
