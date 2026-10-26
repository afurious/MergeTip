# Runbook: disable payments globally (SEV1)

Use when money may be moving wrongly: a double payment (`TipPaidTwice`), a suspected key or
wallet-credential leak, a payer bug, or anything you can't explain. Pausing is always safe:
nothing is lost, tips wait in `paying` and resume when payments are turned back on.

## Symptoms

- Alert `TipPaidTwice` (`tips_settled_more_than_once > 0`), SEV1.
- Reports of tips paid to the wrong person or the wrong amount.
- A suspected leak of `MASTER_KEY`, the database, or NWC connection strings.

## Check

- `tips_settled_more_than_once`, `tips_final_total`, `nwc_errors_total` on the dashboard.
- `SELECT tip_id, count(*) FROM payment_attempts WHERE status = 'succeeded' GROUP BY 1 HAVING count(*) > 1;`

## Steps

1. **Stop payments** (takes effect on the next `pay_tip` run; nothing else is needed):

   ```sql
   UPDATE feature_flags SET enabled = false WHERE key = 'payments_enabled';
   ```

   Paying tips are retried every 5 minutes while the flag is off. Note that a tip whose job runs
   out of attempts while paused is failed (known issue, M3 review); keep the pause short or
   re-queue those tips afterwards.
2. **Stop fiat payouts** too: `UPDATE feature_flags SET enabled = false WHERE key = 'fiat_payouts';`
3. **If credentials may have leaked**, revoke every wallet connection (senders must reconnect;
   they should also revoke the connection in their wallet app):

   ```sql
   UPDATE wallet_connections SET revoked_at = now() WHERE revoked_at IS NULL;
   ```

   Then rotate `MASTER_KEY` ([key-rotation.md](key-rotation.md)) and the GitHub App secrets.
4. Post on the status page: "Tips are paused; nothing has been charged twice / we are checking."
5. Investigate with `tip_events` (every transition, with reasons) and `payment_attempts`.

## Verify recovery

1. Fix, deploy, then turn payments back on:
   `UPDATE feature_flags SET enabled = true WHERE key = 'payments_enabled';`
2. Watch `tips_final_total{state="paid"}` rise and `tips_in_state{state="paying"}` drain within
   minutes; `tips_settled_more_than_once` stays 0.
