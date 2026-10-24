# RFC 0002: Paying tips

- Author: @Xoulomon
- PRD requirement(s): FR-5.1–5.4, FR-6.1–6.4, FR-4.4, "Security, privacy and abuse prevention"
- Status: in review

## Problem

A verified tip in `paying` must move sats from the sender's own wallet (NWC) to the receiver's
Lightning address (LNURL-pay). It must:

- pay exactly once;
- never pay an invoice that doesn't match the tip;
- never exceed the sender's caps;
- never lose track of a payment whose outcome is unknown (a timed-out wallet may still have paid).

## Proposal

### Pay sequence (`pay_tip` job)

1. **Preconditions.**
   - If the `payments_enabled` flag is off, retry later and log a warning.
   - Load the tip and require state `paying`.
   - Load the receiver. With no Lightning address, move `paying → pledged`.
2. **Invoice, outside any transaction.** Resolve the LUD-16 address, request an invoice for exactly
   `amount × 1000` msat (with the note as the LUD-12 comment if allowed), and validate it:
   - amount equals the tip;
   - currency is `LIGHTNING_NETWORK`;
   - signature is valid (checked during parsing);
   - at least 120 s until expiry;
   - `description_hash == sha256(metadata)`.

   Any failure stops here, with no attempt row and nothing sent to the wallet.
3. **Reserve, in one transaction.**
   - `SELECT … FOR UPDATE` the tip and the sender's user row; the user lock serialises cap checks
     per sender.
   - Re-check the state.
   - Check the caps:
     - `amount ≤ per_tip_max`;
     - `reserved_today + amount ≤ daily_max`, where `reserved_today` sums today's (UTC) attempts
       in `pending`, `unknown` and `succeeded`.
   - Insert a `pending` attempt with `payment_hash`, `bolt11`, `amount_msat` and
     `invoice_expires_at`.
   - Set `tips.payment_hash`, then commit.
4. **Pay.** Only the worker decrypts the NWC URI. It sends `pay_invoice` and waits up to 60 s.
5. **Record.**

| Outcome | Attempt | Tip | Next |
| --- | --- | --- | --- |
| Success, `sha256(preimage) == hash` | `succeeded` | `paid` (preimage stored) | Receipt, 🚀 |
| Success, preimage doesn't match | `unknown` | stays `paying` | `reconcile_payment` |
| Terminal wallet error | `failed` | `failed` (error code) | Error reply, 😕 |
| Retryable wallet error | `failed` | stays `paying` | Job retry (up to 3 attempts in total), then `failed` |
| Timeout or relay failure after sending | `unknown` | stays `paying` | `reconcile_payment` |

### Wallet errors (NIP-47)

| Code | Class | Bot message |
| --- | --- | --- |
| `QUOTA_EXCEEDED` | terminal | Your wallet's budget is used up |
| `INSUFFICIENT_BALANCE` | terminal | Your wallet doesn't have enough sats |
| `RESTRICTED`, `UNAUTHORIZED` | terminal | MergeTip isn't allowed to pay from this wallet any more |
| `NOT_IMPLEMENTED` | terminal | Your wallet doesn't support paying invoices |
| `RATE_LIMITED`, `PAYMENT_FAILED`, `INTERNAL`, `OTHER` | retryable | (after retries) The payment failed |

### Reconciliation (`reconcile_payment`)

1. If the wallet listed `lookup_invoice`, look the payment up by hash.
   - **Settled with a valid preimage:** the attempt succeeds and the tip is paid.
   - **Failed:** the attempt fails; retry if attempts remain, otherwise the tip fails.
2. If the payment is still pending, or `lookup_invoice` isn't supported, reschedule at
   `min(now + 30 s × 2ⁿ, invoice_expires_at + 60 s)`.
3. After the invoice expires with no settlement, the payment can no longer complete. The attempt
   fails and a **new** attempt with a **new** invoice is allowed.

### Double-pay guards

- A partial unique index allows at most one `pending`/`unknown` attempt per tip, so a second
  payment can't start while one may be in flight.
- A partial unique index allows at most one `succeeded` attempt per tip.
- `tips.payment_hash` is unique.
- The job lease is renewed while `pay_invoice` waits (M1). A crashed worker's payment is found by
  reconciliation, never re-sent blindly.

### Accounts and sessions

- GitHub App user-to-server OAuth, with the callback served by the API. The access and refresh
  tokens are envelope-encrypted.
- Web sessions: an `mt_session` cookie (`HttpOnly`, `SameSite=Lax`, `Secure` outside dev), 30 days,
  sliding.
- Extension tokens: bearer, 7 days.
- Both are stored only as SHA-256 hashes.
- Cookie-authenticated writes must carry an `Origin` equal to the web app's origin (CSRF).

## Data model / API changes

- New tables:
  - `sessions`
  - `payment_attempts` (with the two partial unique indexes)
- API (all already in the frozen contract):
  - `/auth/github/login`, `/auth/github/callback`, `/auth/logout`, `/auth/extension-token`
  - `GET /me`
  - `PUT /me/wallet`, `DELETE /me/wallet`
  - `PUT /me/lightning-address`
  - `PATCH /me/settings`
- The GitHub App manifests' OAuth callback moves to the API, and the App requests
  `email_addresses: read`.

## Security and payment-safety impact

| Threat (PRD table) | Covered by |
| --- | --- |
| Malicious Lightning address returns a wrong invoice | Validation in step 2, before any attempt row exists |
| Database leak exposes NWC secrets | Envelope encryption; the URI is never logged, echoed or returned |
| Compromised server drains wallets | Backend caps checked under the user row lock, on top of the wallet's own budget |
| Webhook replay double-pays | Unique attempt indexes and reconciliation |

## Alternatives considered

- **The `nwc` crate.** It wraps wallet error codes in an opaque error. The transport is instead
  built on `nostr-sdk` with `nostr`'s NIP-47 types, so codes map exactly.
- **`lnurl-rs`.** Rejected in favour of `reqwest`: it gives control over http/https policy and
  timeouts, and is easy to test with wiremock.
- **Holding a DB transaction across `pay_invoice`.** Rejected: it would pin a connection for up to
  60 s and does nothing for crash safety. The attempt row does that.

## Rollout (feature flag, migration plan)

- Payments are behind the seeded `payments_enabled` flag, which can be switched off instantly.
- `STUB_PAYER` keeps working on staging until a real wallet is connected.
- The migration is additive.
