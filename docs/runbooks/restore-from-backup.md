# Runbook: restore the database from a backup

The database holds no funds, but it holds what proves payments happened (preimages,
`payment_attempts`) and what prevents paying twice (idempotency keys, open attempts). Restore
carefully: a restore that loses recent rows can make the system forget a payment in flight.

## Symptoms

Data loss or corruption: dropped table, bad migration, provider incident.

## Steps

1. **Disable payments first** ([disable-payments.md](disable-payments.md)) and scale the worker
   to zero: `fly scale count 0 -a mergetip-worker-production`. Keep the api up only if it can
   still store webhooks; otherwise scale it down too (GitHub retries deliveries for a while and
   they can be redelivered from the App settings).
2. Restore into a **new** database from the provider's point-in-time backup, to just before the
   incident (Fly Postgres: `fly postgres` snapshot restore; managed providers: PITR).
3. Run migrations against it: `DATABASE_URL=<new> mergetip-api migrate`.
4. Before pointing the apps at it, reconcile anything in flight at the restore point:
   - `SELECT id FROM tips WHERE state IN ('paying', 'fiat_processing');` and their
     `payment_attempts`. For each attempt in `pending`/`unknown`, the reconcile job asks the
     sender's wallet (`lookup_invoice`) when the worker starts: leave them, do not delete them.
   - Tips paid after the restore point exist only in wallets and GitHub receipts. Compare the
     bot's receipts posted after the restore point with `tips.state`; mark any missing as
     incidents rather than letting them be paid again (they can't be: their comment's
     idempotency key exists only if the row does, so re-check them by hand before redelivering
     webhooks).
5. Point `DATABASE_URL` (Fly secret) at the new database, scale the worker back up, re-enable
   payments.

## Verify recovery

`tips_in_state` looks sane, `jobs_ready` drains, `tips_settled_more_than_once` is 0, a test tip on
staging-equivalent data pays end to end.
