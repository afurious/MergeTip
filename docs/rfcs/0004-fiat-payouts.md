# RFC 0004: Fiat payouts through Bitnob

- Author: backend
- PRD requirement(s): FR-10, FR-12.7, "Bitnob payout flow"
- Status: **withdrawn** (2026-10-27). Fiat payouts were removed before release: receivers are
  paid over Lightning only. The code, schema (migration `remove_fiat`), endpoints, alert and
  runbooks are gone; this RFC stays as the record of the design.

## Problem

Receivers in Nigeria, Ghana and Kenya often want local currency in a bank account or mobile-money
wallet, not sats. Bitnob's Payouts API delivers local currency and can be funded per payout by
paying an invoice ("on-the-fly settlement"), so MergeTip can stay non-custodial: the sender's own
NWC wallet pays Bitnob, and Bitnob pays the receiver.

## What the Bitnob docs confirm (read 2026-10-26)

| Topic | Confirmed | Source |
| --- | --- | --- |
| Base URL | `https://api.bitnob.com` (production). No sandbox URL is documented; `.env.example` keeps `https://sandboxapi.bitnob.co` from the scaffold | `/api-reference/authentication` |
| Request signing | Headers `X-Auth-Client`, `X-Auth-Timestamp` (Unix seconds), `X-Auth-Nonce` (16 random bytes, hex), `X-Auth-Signature` = hex(HMAC-SHA256(`CLIENT_SECRET`, `"{CLIENT_ID}:{TIMESTAMP}:{NONCE}:{BODY}"`)), where `BODY` is the exact JSON sent (empty string without a body) | same |
| Quote | `POST /api/payouts/quotes`; body `from_asset`, `to_currency`, `source` (`onchain`/`offchain`), `amount` or `settlement_amount`, `chain`, `payment_reason`, `reference`, `country`; response `id`, `quote_id`, `status`, `settlement_amount`, `exchange_rate.rate`, `amount`, `expires_at`, `fees` | `/api-reference/payouts`, payouts guide |
| Initialize | `POST /api/payouts/:quoteId/initialize`; body `customer_id`, `beneficiary` (`destination_type`, `country`, `account_name`, `account_number`, `bank_code`, `network` for mobile money), `reference`, `payment_reason`, `callback_url`; response `data.payout` with `expires_at` | same |
| Get payout | `GET /api/payouts/:id`; statuses `QUOTE`, `INITIATED`, `PENDING`, `PROCESSING`, `COMPLETED`, `FAILED` | `/api-reference/payouts/list-payouts` |
| Countries | `GET /api/payouts/supported-countries`, `GET /api/payouts/supported-countries/:country` (rails with `fields[]`: `key`, `label`, `type`, `required`, `component`, optional `pattern`, `min_length`, `max_length`, `placeholder`, `options`; `banks`; `limits`) | `/api-reference/payouts/get-supported-countries` |
| Limits | `GET /api/payouts/limits` → `data.limits[]` with `country`, `currency`, `min_amount`, `max_amount` | same |
| Account lookup | `GET /api/payouts/account-lookup?country=&type=&bank_code=&account_number=` → `data.account_name`, `data.bank_name`, `data.is_verified` | `/api-reference/payouts/account-lookup` |
| Webhooks | Events `payouts.initialized`, `payouts.processing`, `payouts.withdrawal.success`, `payouts.withdrawal.expired`; body `{event, event_id, data: {id, reference, status, …}}`; header `x-bitnob-signature`, HMAC-SHA512 of the payload with the secret key; `x-bitnob-event-id`; route on `event`, dedupe on `event_id` | `/api-reference/payouts/webhooks` |

## What is assumed (marked `// ASSUMPTION(bitnob)` in code, tracked in `BLOCKERS.md`)

1. **Lightning funding.** The settlement-models page says on-the-fly payouts are funded by "a
   generated Bitcoin, Lightning, or USDT address/invoice", but no request value selecting
   Lightning is documented (`source` is `onchain`/`offchain`). We send `source: "onchain"`,
   `chain: "LIGHTNING"` and read the invoice from `data.payout.payment_request` (falling back to
   `payment_address`). If the sandbox disagrees, only `BitnobClient` changes.
2. **Webhook signature encoding:** hex, over the raw request body.
3. **`customer_id`:** a Bitnob customer per receiver is required by Initialize; we create none and
   store `provider_customer_id` when known. Registering customers (and any KYC) waits for the
   business account.
4. **Amount units:** `amount` in BTC as a decimal string for `from_asset: "btc"`.
5. **Sandbox URL.**

No field is invented silently: every assumed name is next to an `ASSUMPTION(bitnob)` comment and
a `wiremock` fixture, so a sandbox run tells us exactly which lines to change.

## Proposal

### Data model

- `payout_methods`: one row per fiat destination (`kind = bitnob_fiat`), with `country`,
  `currency`, `destination_type` (`bank`/`mobile_money`), the beneficiary envelope-encrypted with
  AAD `payout_methods.beneficiary:{id}`, `verified_at` (Account Lookup passed), and at most one
  `is_default` per user (partial unique index). The Lightning address stays in
  `users.lightning_address`; it is the default when no fiat method is, and the fallback when a
  tip is too small for the fiat corridor.
- `tips`: `fiat_currency`, `fiat_amount`, `fx_rate`, `fees` (`NUMERIC` ↔ `rust_decimal`, never
  floats), `provider_quote_id`, `provider_payout_id` (unique), `provider_status`.
- `bitnob_events (event_id pk, payload, received_at, processed_at)`: webhook dedupe and audit.

### Pay sequence (`pay_tip`, receiver's default method is fiat, flags on)

1. Corridor limits (`GET /limits`, cached per process for 10 min). Below the minimum: pay the
   receiver's Lightning address if set, else pledge (FR-10 "smaller tips").
2. Quote with `reference = tip_id`, `amount = tip in BTC`.
3. Initialize with the decrypted beneficiary, a unique reference `"{tip_id}:{attempt}"` and
   `callback_url = {API}/v1/webhooks/bitnob`.
4. Validate the invoice exactly like an LNURL one: amount equals the quoted amount, at least
   120 s left, right network, valid signature (no description-hash check: Bitnob's invoice
   commits to no LNURL metadata).
5. Reserve the cap and pay over NWC, as in RFC 0002 (attempt row first, never blind-retry).
6. Quote or invoice expired before payment: re-quote **once**; a second expiry fails the tip with
   `fiat_quote_expired`.
7. NWC success: `paying → fiat_processing` (the preimage proves Bitnob was paid). The tip is
   `paid` only on `payouts.withdrawal.success`; the receipt then says "paid out in local
   currency" and never shows the amount, currency or destination.
8. NWC timeout: reconcile exactly as in RFC 0002.

### Webhooks

`POST /v1/webhooks/bitnob`: verify `x-bitnob-signature` over the raw body in constant time, insert
`bitnob_events` with `ON CONFLICT DO NOTHING`, enqueue `process_bitnob_event`, answer 200.
Processing finds the tip by `provider_payout_id` (or `reference`) and moves by provider status
rank (`initiated < processing < success|expired|failed`), so late or reordered events never move a
tip backwards:

- `payouts.processing`: record `provider_status`.
- `payouts.withdrawal.success`: `fiat_processing → paid`, receipt.
- `payouts.withdrawal.expired` / failed: `fiat_processing → fiat_stuck`.

### Stuck payouts (the only time value sits outside a user wallet)

`fiat_stuck` means Bitnob holds the sats but delivery failed. It is alerted on
(`tips_in_state{state="fiat_stuck"} > 0` for 30 min, SEV2) and resolved by an operator within
24 h with `mergetip-worker ops fiat-retry <tip>` (back to `fiat_processing`, re-poll) or
`mergetip-worker ops fiat-refund <tip> --bolt11 <invoice>` (records the refund invoice; `→
refunded`). Every ops action records `tip_events.detail.operator`. Runbook:
`docs/runbooks/fiat-stuck.md`.

## Security and payment-safety impact

- Beneficiary data (account numbers, phone numbers, names) is encrypted at rest, `Redacted` in
  memory, never logged or returned; responses show only the last 4 digits.
- The double-pay guards of RFC 0002 apply unchanged: one open attempt per tip, one success.
- Webhooks are signed and deduplicated; a forged or replayed event cannot mark a tip paid.
- Fees are deducted from the tip (PRD default); the sender pays exactly the tip amount.

## Alternatives considered

- Holding a MergeTip Bitnob balance to batch small tips: rejected in the PRD (custodial).
- Kotani Pay: kept as a second `FiatProvider` implementation behind the same trait if Bitnob KYB
  stalls.

## Rollout

`FIAT_PAYOUTS_ENABLED` (env, process-wide) **and** the `fiat_payouts` flag row must both be on.
Until the business account, KYB and sandbox confirm the assumptions above, both stay off in every
environment; the code paths run in tests against `wiremock` fixtures only.
