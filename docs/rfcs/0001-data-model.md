# RFC 0001: Initial data model

- Author: @Xoulomon
- PRD requirement(s): FR-4.1, FR-4.2, FR-4.4, FR-11.8, FR-12.8, "System architecture and data model"
- Status: in review

## Problem

Every later milestone writes to the same few tables. The schema has to make double payment
impossible at the database level, keep a full audit trail of every tip, and store secrets so that a
database dump alone reveals nothing usable. It should also need only additive migrations from here
to launch.

## Proposal

One `init` migration creates the tables below. Payment attempts and sessions (M3), OTP codes and
email deliveries (M4), and fiat payout tables (M5) arrive later as separate additive migrations.

| Table | Purpose | Key constraints |
| --- | --- | --- |
| `users` | One row per GitHub account, sender or receiver, plus their caps and settings | `github_id` unique; caps `> 0`; `confirm_threshold_sats >= 1000` |
| `installations` / `installation_repos` | Where the App is installed; cached `.github/mergetip.yml` | `github_installation_id` unique |
| `github_events` | Raw webhook deliveries | `delivery_id` primary key (deduplicates redeliveries) |
| `wallet_connections` | Encrypted NWC connection per sender | At most one row per user with `revoked_at IS NULL` |
| `tips` | One row per `/tip` command | `idempotency_key` unique; `payment_hash` unique; `claim_token_hash` unique |
| `tip_events` | Append-only audit of every state transition | — |
| `bot_comments` | The single collapsed status comment per thread | Primary key `(repo, thread_number)` |
| `jobs` | Postgres job queue | Partial unique `(kind, dedupe_key)` among live jobs |
| `feature_flags` | Runtime switches such as `payments_enabled` | — |

### State storage

`tips.state` is `TEXT` with a `CHECK` listing every `core::TipState` value. A Postgres enum would
need `ALTER TYPE` for each new state, and that can't run inside a transaction on older servers. A
`CHECK` constraint can be swapped in one additive migration. The Rust enum is the source of truth,
and a test asserts the two lists match.

Only `store::tips::transition` writes `state`. It validates the change with `core::transition`, then
runs a compare-and-set:

```sql
UPDATE tips SET state = $to, version = version + 1, updated_at = now()
WHERE id = $id AND state = $from
```

If no row is updated, another worker got there first, and the caller gets `Conflict`. The
`tip_events` row is written in the same transaction, so the audit log can never disagree with the
tip.

### Idempotency

`idempotency_key = hex(sha256("{github_installation_id}:{comment_id}:{command_index}"))`, inserted
with `ON CONFLICT (idempotency_key) DO NOTHING`. GitHub redelivers the same comment under new
delivery IDs, so `github_events.delivery_id` alone is not enough; the key is derived from the
comment itself. Edited comments are ignored, so a comment's command list never changes after the
first time it is seen.

### Encryption format

Encrypted columns are `bytea` blobs produced by `crypto::Keyring::seal`:

```
version(1) ‖ key_id(1) ‖ dek_nonce(12) ‖ wrapped_dek(48) ‖ data_nonce(12) ‖ ciphertext+tag
```

Each value has its own AES-256-GCM data key, wrapped by `MASTER_KEY`. The AAD is
`"{table}.{column}:{row_id}"`, so a ciphertext moved to another row or column fails to decrypt.
Because the data key lives inside the blob, the PRD's separate `data_key_ciphertext` column is
dropped. Rotating the master key re-wraps only the 48-byte data key.

### Job queue

`jobs` rows are claimed with `FOR UPDATE SKIP LOCKED` and a 5-minute lease. Successful jobs are
deleted. A job that exhausts `max_attempts` gets `dead_at` and stays in the table for inspection.
`dedupe_key` is unique among live jobs, which gives coalescing for recurring and digest jobs.
Enqueuing calls `pg_notify('jobs', kind)` so idle workers wake up immediately.

## Data model / API changes

This RFC introduces the schema. It makes no API changes.

## Security and payment-safety impact

- **Double payment:** blocked by the unique `idempotency_key`, the compare-and-set transition, the
  unique `payment_hash`, and (M3) a partial unique index allowing one succeeded attempt per tip.
- **Secrets at rest:** NWC URIs, GitHub user tokens and emails are encrypted with AAD binding.
  Opaque tokens are stored only as SHA-256 hashes.
- **Audit:** `tip_events` records every transition with JSON detail. It has no update path.

## Alternatives considered

- **Postgres enum for state:** rejected because adding states needs migration gymnastics.
- **External queue (Redis, SQS):** rejected for v1. Enqueuing in the same transaction as the state
  change is what makes the queue exactly-once with respect to state, and only Postgres gives that
  for free.
- **Separate data-key column:** rejected because one blob is simpler and lets rotation stay atomic
  per value.

## Rollout (feature flag, migration plan)

The `init` migration runs on an empty database. `feature_flags` is seeded with
`payments_enabled = true` and `fiat_payouts = false`. All later changes are expand-only migrations.
