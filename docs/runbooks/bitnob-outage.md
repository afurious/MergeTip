# Runbook: Bitnob outage

Fiat payouts (RFC 0004) depend on Bitnob's API and webhooks. Lightning tips are unaffected.

## Symptoms

- Tips to fiat receivers failing with `fiat_rejected` or `fiat_unavailable`, or stuck in
  `paying` with `pay_tip` retries; `fiat_processing` tips not completing.
- `FiatPayoutStuck` firing for several tips at once.
- Bitnob status page or support reports an incident.

## Check

1. `SELECT state, error_code, count(*) FROM tips WHERE fiat_currency IS NOT NULL AND updated_at > now() - interval '6 hours' GROUP BY 1, 2;`
2. Worker logs for `retrying Bitnob call` and `payout provider unavailable`.
3. Last webhook received: `SELECT max(received_at) FROM bitnob_events;`

## Steps

1. Pause fiat payouts so new tips use the receiver's Lightning address or become pledges
   instead of failing:

   ```sql
   UPDATE feature_flags SET enabled = false WHERE key = 'fiat_payouts';
   ```

   (Receivers with only a fiat method get a pledge; nothing is charged.)
2. Tips already `fiat_processing` keep their state; when Bitnob recovers, its webhooks (retried
   by Bitnob) move them on. If webhooks were lost, re-poll each one:
   `/app ops fiat-retry` only works on `fiat_stuck`, so for `fiat_processing` tips
   wait for Bitnob, then handle any that end up stuck with [fiat-stuck.md](fiat-stuck.md).
3. Post on the status page.

## Verify recovery

Turn `fiat_payouts` back on; new fiat tips reach `fiat_processing` then `paid`;
`tips_in_state{state="fiat_stuck"}` is 0.
