# Runbooks

One per alert in [`deploy/observability/alerts.yml`](../../deploy/observability/alerts.yml), plus
the operations that need a checklist. Each has symptoms, what to check, numbered steps and how
to verify recovery.

| Runbook | Alerts | When |
| --- | --- | --- |
| [disable-payments.md](disable-payments.md) | `TipPaidTwice` (SEV1) | Money may be moving wrongly; the global kill switch |
| [payments-failing.md](payments-failing.md) | `TipSuccessRateLow`, `TipLatencyHigh`, `JobQueueStalled`, `EmailBounceRateHigh` | Tips failing, slow or stuck in the queue |
| [relay-outage.md](relay-outage.md) | `NwcTimeoutsHigh` | Wallets unreachable over Nostr relays |
| [github-outage.md](github-outage.md) | `WebhookAckSlow`, `GitHubRateLimitLow` | GitHub down, slow or rate limiting |
| [bitnob-outage.md](bitnob-outage.md) | — | Fiat payouts failing |
| [fiat-stuck.md](fiat-stuck.md) | `FiatPayoutStuck` | Sats with Bitnob, fiat not delivered (resolve within 24 h) |
| [key-rotation.md](key-rotation.md) | — | Rotating `MASTER_KEY` |
| [restore-from-backup.md](restore-from-backup.md) | — | Database restore |

Sentry's "new error type" alerting (implementation plan) is configured in Sentry itself, per
project, when `SENTRY_DSN` is set.
