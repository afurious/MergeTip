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
- [ ] Handover

## M4: Edge flows and notifications

Not started.

## M5: Fiat payouts and hardening

Not started.
