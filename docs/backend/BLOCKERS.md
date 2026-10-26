# Backend blockers

External dependencies (secrets, sandboxes, decisions) that stop work from being finished for
real. Each entry says what is stubbed in the meantime and what unblocks it.

| Date | Blocker | Workaround in place | Unblocked by |
| --- | --- | --- | --- |
| 2026-10-26 | M3 manual check: one 21-sat mainnet payment on staging between two team wallets (Alby or Coinos) | Not done, and not faked. The full NWC + LNURL round trip is covered offline by the mock wallet, `mock-lnurl` and the e2e suite | Two funded team wallets with NWC connection strings, a deployed staging api and worker with `LIGHTNING_NETWORK=bitcoin`, and someone to run the payment and record it in `handover/m3.md` |
| 2026-10-26 | M4 manual check: pledge, reminder and OTP email inbox placement in Gmail and Outlook | Rendering verified in Mailpit; Resend path covered by wiremock tests | A Resend account and a sending domain with SPF, DKIM and DMARC, then a staging run sending to Gmail and Outlook test inboxes |
| 2026-10-26 | Bitnob business account, KYB and sandbox credentials (FR-10) | Fiat payouts stay behind `FIAT_PAYOUTS_ENABLED` and the `fiat_payouts` flag, both off; `BitnobClient` runs only against `wiremock` fixtures | A Bitnob business account with sandbox `BITNOB_CLIENT_ID`/`BITNOB_SECRET` and webhook secret |
| 2026-10-26 | Bitnob API details the public docs don't state (RFC 0004 "What is assumed"): the request value that funds a payout over Lightning, the invoice field in the Initialize response, webhook signature encoding, `customer_id` creation, the sandbox base URL, a signing test vector | Each assumption is marked `// ASSUMPTION(bitnob)` in `crates/fiat` with a fixture; the signing vector is self-computed from the documented algorithm | One sandbox run (or Bitnob support) confirming each; only `crates/fiat` changes |
