# ADR 0002: Backend crate layering

- Status: accepted
- Date: 2026-10-24

## Context

The backend is a Cargo workspace of many crates (ADR 0001). Payment safety depends on a few rules
holding everywhere: one code path changes tip state, secrets never reach logs, and business logic
can be tested without GitHub, a Nostr relay or a Lightning wallet. Those rules are easiest to keep
when the dependency direction between crates is fixed and obvious.

## Decision

Crates are layered. A crate may depend only on crates in the layers below it.

| Layer | Crates | May do I/O | Notes |
| --- | --- | --- | --- |
| Domain | `core`, `parser` | No | Tip states and transitions, money newtypes, IDs, `Clock`, the `/tip` grammar |
| Primitives | `crypto` | No (OS randomness only) | Envelope encryption, `Redacted<T>`, HMAC, tokens |
| Persistence | `store` | Postgres | Repositories and the job queue. `store::tips::transition` is the only writer of `tips.state` |
| Adapters | `github`, `payer`, `notify`, `fiat` | External services | Each exposes a trait plus a real implementation, so tests swap in fakes |
| Use cases | `engine` (from M2) | Through the traits above | Tip creation, verification, payment and expiry flows, shared by both binaries |
| Process plumbing | `runtime` | Env, signals, Postgres | Typed config, tracing setup, pool and migrations, shutdown token |
| Binaries | `api`, `worker` | Yes | Thin: parse input, call `engine` or `store`, map results. No business rules |

Specific decisions:

1. **`engine` arrives in M2.** Both `api` (claims, confirmations) and `worker` (payments, expiry)
   run tip use cases. Putting them in a shared crate keeps one implementation of each flow.
2. **`runtime` is shared plumbing.** Both binaries load config, initialise tracing, connect to
   Postgres and handle SIGTERM the same way. Hand-rolled env parsing (`runtime::config::Env`)
   avoids a dependency and lets tests pass a `HashMap` instead of mutating process env.
3. **`api` holds `MASTER_KEY` but never decrypts NWC URIs.** The API has to *encrypt* a wallet
   connection when a user connects it, and *decrypt* user emails and GitHub tokens. Only the
   worker decrypts NWC URIs, the one secret that can move money. The rule is enforced by review:
   the NWC decrypt call site lives only in `worker` and `engine` payment code. Splitting keys (a
   KMS, or an asymmetric key so the API can encrypt without decrypting) is a hardening follow-up.
4. **Time is injected.** Code that compares against "now" takes a `core::Clock`, so expiry windows
   are tested with `FixedClock` instead of sleeps.
5. **Database time vs. application time.** The job queue compares `run_at` and lease ages with
   Postgres `now()`, so every worker agrees on one clock. Domain deadlines (`expires_at`,
   `confirm_expires_at`) are computed from `Clock` in application code.

## Consequences

- Adding an integration means a trait in its adapter crate and a fake for tests. `engine` never
  names a concrete client type.
- `api` and `worker` stay small enough to read in one sitting. New behaviour shows up in `engine`
  or an adapter, where it is unit-testable.
- The single-writer rule for `tips.state` is checked by a test in `store` that fails if any other
  Rust file contains an assignment to `state`.
