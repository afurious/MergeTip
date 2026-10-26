# Backend blockers

External dependencies (secrets, sandboxes, decisions) that stop work from being finished for
real. Each entry says what is stubbed in the meantime and what unblocks it.

| Date | Blocker | Workaround in place | Unblocked by |
| --- | --- | --- | --- |
| 2026-10-26 | M3 manual check: one 21-sat mainnet payment on staging between two team wallets (Alby or Coinos) | Not done, and not faked. The full NWC + LNURL round trip is covered offline by the mock wallet, `mock-lnurl` and the e2e suite | Two funded team wallets with NWC connection strings, a deployed staging api and worker with `LIGHTNING_NETWORK=bitcoin`, and someone to run the payment and record it in `handover/m3.md` |
