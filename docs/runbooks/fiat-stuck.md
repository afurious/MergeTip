# Runbook: stuck fiat payout (`fiat_stuck`)

**Severity:** SEV2. **Deadline:** resolve within 24 h of the alert (PRD "Fiat payout states").

A `fiat_stuck` tip is the one case where value sits outside a user wallet: the sender's wallet
paid Bitnob's invoice (the tip has a preimage), but Bitnob reported the local-currency payout as
expired or failed. Bitnob now holds the sats in MergeTip's balance. Either get the payout
delivered, or refund the sender over Lightning.

## Symptoms

- Alert `FiatPayoutStuck`: `max(tips_in_state{state="fiat_stuck"}) > 0` for 30 minutes.
- Worker log line `fiat payout stuck: sats with the provider, fiat not delivered` with a `tip_id`.
- A sender or receiver reports "tip paid, money never arrived".

## Check

1. List stuck tips:

   ```sql
   SELECT id, amount_sats, fiat_currency, fiat_amount, provider_payout_id, provider_status,
          error_code, updated_at
   FROM tips WHERE state = 'fiat_stuck' ORDER BY updated_at;
   ```

2. See the payout's history and the webhooks that arrived:

   ```sql
   SELECT from_state, to_state, detail, at FROM tip_events WHERE tip_id = '<tip>' ORDER BY id;
   SELECT event_id, payload->>'event', received_at FROM bitnob_events
   WHERE payload->'data'->>'id' = '<provider_payout_id>' ORDER BY received_at;
   ```

3. Look the payout up in the Bitnob dashboard by `provider_payout_id`: did Bitnob receive the
   sats, and why did delivery fail (wrong account, rail outage, compliance hold)?

## Fix

Run on a worker machine (`fly ssh console -a mergetip-worker-production`; the worker binary is
`/app` in the image), with your name as
the operator. Every command is recorded in the tip's audit trail.

### A. Bitnob delivered after all, or delivery can be retried

1. If Bitnob support re-sends the payout, wait until their dashboard shows it completed.
2. Reopen the tip and re-poll Bitnob:

   ```sh
   /app ops fiat-retry <tip_id> --operator <your-name>
   ```

   The log line `operator action done` shows the result: `paid` (the receipt is posted in the
   thread), `fiat_processing` (still in progress; Bitnob's webhook finishes it), or `fiat_stuck`
   (still failed: go to B).

### B. Refund the sender

1. Ask the sender (GitHub thread or support email) for a Lightning invoice for **exactly** the tip
   amount (`amount_sats`), valid for at least 10 minutes.
2. Pay it from MergeTip's Bitnob balance in the Bitnob dashboard.
3. Record the refund:

   ```sh
   /app ops fiat-refund <tip_id> --bolt11 <invoice> --operator <your-name>
   ```

   The command refuses an invoice for any other amount or network.

## Verify recovery

- `SELECT state FROM tips WHERE id = '<tip>'` is `paid` or `refunded`.
- `tips_in_state{state="fiat_stuck"}` drops to 0 and the alert resolves within the next scrape.
- `tip_events` has the row with `detail.operator` set to you.

## Follow-up

If the cause was the receiver's account (closed, wrong name), ask them to update their payout
method; until they do, set their default back to Lightning (`PATCH /v1/me/payout-methods/{id}`
with `is_default: false`, as them or via support).
