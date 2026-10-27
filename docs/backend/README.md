# Backend

Rust services for MergeTip: the HTTP API (`mergetip-api`) and the job runner
(`mergetip-worker`), backed by Postgres. See [ADR 0002](../adr/0002-backend-layering.md) for how
the crates fit together, and the RFCs for each area: [0001 data model](../rfcs/0001-data-model.md),
[0002 payments](../rfcs/0002-payments.md), [0003 pledges, confirmations and
notifications](../rfcs/0003-pledges-confirmations-notifications.md). ([0004 fiat
payouts](../rfcs/0004-fiat-payouts.md) was withdrawn: receivers are paid over Lightning only.)

## Architecture in one picture

```
GitHub ──signed webhook──▶ api ──store + enqueue (one tx)──▶ Postgres ◀──claim (SKIP LOCKED)── worker
web app / extension ──REST──▶ api                                                          │
                                                                                            ├─ GitHub App (verify, reply)
                                                                                            ├─ LNURL (receiver invoice)
                                                                                            ├─ NWC over Nostr (sender's wallet pays)
                                                                                            └─ Resend / SMTP (pledge emails)
```

- The api only validates, stores and queues; every side effect runs in the worker, from jobs
  committed in the same transaction as the state change that needs them.
- `store::tips::transition` is the only writer of `tips.state` (a compare-and-set plus an audit
  row in `tip_events`); a test fails if any other SQL writes it.
- Money moves only from `paying`, through one open `payment_attempts` row at a time; a
  timed-out payment is reconciled with the wallet, never retried blindly. MergeTip holds no
  funds: the sender's own wallet pays the receiver.
- Secrets are envelope-encrypted per value (AAD `"{table}.{column}:{id}"`), `Redacted` in memory,
  and never logged (`e2e/tests/log_scan.rs` proves it).

## Prerequisites

Rust stable (see `backend/rust-toolchain.toml`), Docker, and `sqlx-cli`
(`make bootstrap` installs it).

## Run it

```sh
cp .env.example .env                 # then set MASTER_KEY: openssl rand -hex 32
make up                              # Postgres and the local mocks
make migrate                         # apply migrations
make dev-api                         # http://localhost:8080/v1/health
make dev-worker                      # claims jobs from Postgres
```

To receive real webhooks, create a personal GitHub App once:

```sh
# 1. Create a webhook proxy channel at https://smee.io/new and put it in .env as SMEE_URL.
make dev-app        # opens a local page; approve the App on GitHub; writes .env and .secrets/
make smee           # forwards deliveries to the local api
```

GitHub rejects two things the App needs from a manifest, so they live outside it:

- **Email addresses (read):** sign-in reads `/user/emails` for pledge notices, but manifests
  refuse account permissions. After creating the App, open its settings, then
  *Permissions & events → Account permissions → Email addresses: Read-only*, save, and accept the
  change on the installation. Do the same for the staging and production Apps.
- **`installation_repositories` events:** GitHub sends these to every App, so they cannot be listed
  and need no setup.

If port 5432 is already taken on your machine, set `POSTGRES_PORT` in `.env` (e.g. `5435`) and
point `DATABASE_URL` at the same port.

The API binary also prints its OpenAPI document: `cargo run -p mergetip-api -- openapi`.

## Test it

```sh
make check-backend                   # fmt, clippy -D warnings, tests, sqlx offline data
cd backend && cargo test -p mergetip-store      # one crate
```

Database tests use `#[sqlx::test(migrator = "mergetip_store::MIGRATOR")]`. Each test gets a fresh
database created from `DATABASE_URL`, so the compose Postgres must be running.

## Change the schema or a query

1. `make migrate-new name=add_something`, then write expand-only SQL (see
   `backend/migrations/README.md`).
2. `make migrate`.
3. Write the query with `sqlx::query!` / `query_as!` (never string-built SQL).
4. `make sqlx-prepare` and commit `backend/.sqlx/` with the change, so CI and Docker builds
   compile offline (`SQLX_OFFLINE=true`).

## Change the API

1. Add the handler with a `#[utoipa::path]` annotation and list it in `crates/api/src/openapi.rs`.
2. `make openapi` regenerates `openapi/openapi.json` and the frontend's
   `packages/api-client/src/schema.d.ts`. Commit both with the change.
3. Contract changes are additive only; log them in
   [`docs/api-contract-changes.md`](../api-contract-changes.md).

A test in `mergetip-api` fails when `openapi/openapi.json` no longer matches the code.

## Configuration

Every variable is listed with a comment in `.env.example`. Each binary validates its config at
startup and exits with the name of any missing or invalid variable.

| Variable | Used by | Purpose |
| --- | --- | --- |
| `APP_ENV` | both | `dev`, `staging` or `production`; JSON logs outside `dev`; production refuses unsafe settings |
| `DATABASE_URL` | both | Postgres connection string |
| `MASTER_KEY` / `MASTER_KEY_PREVIOUS` | both | Envelope encryption key (32-byte hex) and the old key during rotation |
| `API_ADDR` | api | Listen address |
| `PUBLIC_BASE_URL` | both | Web app origin: the CORS origin and the base for links in replies and emails |
| `API_PUBLIC_URL` | both | This API's origin: OAuth callback and unsubscribe links |
| `EXTENSION_ORIGINS` | api | Browser-extension origins allowed to make cookie-authenticated writes |
| `GITHUB_APP_ID`, `GITHUB_APP_PRIVATE_KEY_PATH` | worker | The GitHub App (installation tokens) |
| `GITHUB_WEBHOOK_SECRET` | api | Webhook signature secret |
| `GITHUB_CLIENT_ID`, `GITHUB_CLIENT_SECRET` | api | Sign in with GitHub |
| `GITHUB_API_URL`, `GITHUB_OAUTH_URL` | both | Overrides for GitHub Enterprise or a mock |
| `LIGHTNING_NETWORK` | both | `bitcoin` in production, `regtest` with the local mocks |
| `STUB_PAYER` | worker | Staging demos without wallets; refused in production |
| `WORKER_CONCURRENCY` | worker | Jobs run in parallel per process |
| `RESEND_API_KEY`, `SMTP_URL`, `EMAIL_FROM` | both | Email: Resend, else SMTP (dev only), else none |
| `RESEND_WEBHOOK_SECRET` | api | Resend delivery events (Svix); unset disables the endpoint |
| `METRICS_ADDR` | both | Prometheus `/metrics` on an internal address |
| `SENTRY_DSN` | both | Error reporting |
| `RUST_LOG` | both | Log filter |

Runtime switches live in the `feature_flags` table: `payments_enabled` (the SEV1 kill switch),
`email_from_commits`.

## Where things live

| Path | What |
| --- | --- |
| `crates/core` | Tip states and transitions, `Sats`/`Msats`, typed IDs, `Clock` |
| `crates/crypto` | `Keyring` envelope encryption, `Redacted<T>`, webhook signatures, tokens |
| `crates/parser` | The `/tip` grammar |
| `crates/github` | GitHub App client, OAuth, verifier, bot reply templates |
| `crates/payer` | LNURL resolution, invoice checks, NWC wallet client, test mocks |
| `crates/notify` | `Mailer` (Resend, SMTP), email templates, unsubscribe tokens |
| `crates/engine` | Every use case: comments, verification, payment, holds, pledges, confirmations, expiry, notices, operator actions |
| `crates/store` | Repositories, `tips::transition`, the job queue |
| `crates/runtime` | Config, tracing and Sentry, metrics exporter, database pool, shutdown |
| `crates/worker` | `Runner`, job handlers, metrics sampler, `ops` commands |
| `crates/api` | Router, auth, rate limits and headers, error contract, OpenAPI |
| `crates/e2e` | The real api and worker against Postgres with faked GitHub, LNURL and wallet |
| `devtools/` | `mock-nwc` and `mock-lnurl` for running everything offline |
| `migrations/` | SQL migrations |
| `.sqlx/` | Offline query metadata for `sqlx` macros |

## Operate it

- **Health:** `GET /healthz` (plain `ok`, for Fly checks) and `GET /v1/health`.
- **Metrics and alerts:** `/metrics` on `METRICS_ADDR` (internal); every alert in
  [`deploy/observability/alerts.yml`](../../deploy/observability/alerts.yml) names its runbook.
- **Runbooks:** [`docs/runbooks/`](../runbooks/README.md).
- **Operator commands** (on a worker machine, `fly ssh console -a mergetip-worker-production`,
  where the binary is `/app`; locally `cargo run -p mergetip-worker --`):
  - `/app ops rewrap --operator <name>`
- **Migrations:** `mergetip-api migrate` (Fly runs it as the api's `release_command`).
- **Critical tests:** [`critical-tests.md`](critical-tests.md); coverage floors via `make coverage`.
- **Deploys:** pushes to `main` deploy staging; `v*` tags deploy production after approval
  (`.github/workflows/deploy-*.yml`).
