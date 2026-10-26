# Backend progress

Ticked when the task's commit lands. Milestones are defined in the backend milestone plan.

## M1: Foundations

- [x] 1.1 Tooling: `check-backend`, `sqlx-prepare`, `migrate-new` targets
- [x] 1.2 `core` domain types and clock
- [x] 1.3 `crypto` envelope encryption
- [x] 1.4 Data model RFC
- [x] 1.5 Initial schema migration
- [x] 1.6 `store` repositories and transitions
- [x] 1.7 Postgres job queue
- [x] 1.8 `worker` runtime
- [x] 1.9 `api` router and error model
- [x] 1.10 OpenAPI pipeline
- [x] 1.11 Layering ADR and backend README
- [x] Handover

## M2: GitHub App core

- [x] 2.1 Parser
- [x] 2.2 Webhook endpoint
- [x] 2.3 `github` client
- [x] 2.4 `engine` crate and event processing
- [x] 2.5 Tip creation
- [x] 2.6 Verifier and decision
- [x] 2.7 Bot replies
- [x] 2.8 Stub payer
- [x] 2.9 End-to-end harness
- [x] 2.10 Dev App helper
- [x] Handover

## M3: Auth and payments

- [x] Payments RFC
- [x] 3.1 Sessions and payment attempts schema
- [x] 3.2 GitHub OAuth sign-in
- [x] 3.3 Auth extractors, sessions and `GET /me`
- [x] 3.4 LNURL resolver and invoice validation
- [x] 3.5 NWC client
- [x] 3.6 Wallet, address and settings endpoints
- [x] 3.7 Real `pay_tip`
- [x] 3.8 Reconciliation
- [x] 3.9 Devtools mocks
- [x] 3.10 Receipts and E2E journey
- [x] Handover

## M4: Edge flows and notifications

- [x] Pledges, confirmations and notifications RFC
- [x] 4.1 On-merge holds
- [x] 4.2 Awaiting sender
- [x] 4.3 Pledges and claims
- [x] 4.4 High-value confirmation
- [x] 4.5 `notify` crate
- [x] 4.6 Pledge notifications
- [x] 4.7 Expiry worker
- [x] 4.8 Remaining endpoints
- [x] 4.9 E2E journeys
- [x] Handover

## M5: Fiat payouts and hardening

- [x] 5.1 Fiat payouts RFC and schema
- [x] 5.2 `fiat` crate (Bitnob client)
- [x] 5.3 Receiver payout API
- [x] 5.4 Fiat pay path
- [x] 5.5 Bitnob webhooks
- [x] 5.6 Stuck-payout operations
- [x] 5.7 Metrics and alerts
- [x] 5.8 Abuse and HTTP hardening
- [x] 5.9 Critical-test audit
- [x] 5.10 Coverage gate
- [ ] 5.11 Container and deploy
- [ ] 5.12 CI gates
- [ ] 5.13 Runbooks and final docs
- [ ] 5.14 Final handover
