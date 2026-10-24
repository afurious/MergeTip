# Backend

Rust services for MergeTip: the HTTP API (`mergetip-api`) and the job runner
(`mergetip-worker`), backed by Postgres. See [ADR 0002](../adr/0002-backend-layering.md) for how
the crates fit together and [RFC 0001](../rfcs/0001-data-model.md) for the schema.

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
| `APP_ENV` | both | `dev`, `staging` or `production`; JSON logs outside `dev` |
| `DATABASE_URL` | both | Postgres connection string |
| `MASTER_KEY` / `MASTER_KEY_PREVIOUS` | both | Envelope encryption key (32-byte hex) and the old key during rotation |
| `API_ADDR` | api | Listen address |
| `PUBLIC_BASE_URL` | api | Web app origin: the CORS origin and the base for links |
| `WORKER_CONCURRENCY` | worker | Jobs run in parallel per process |
| `RUST_LOG` | both | Log filter |

## Where things live

| Path | What |
| --- | --- |
| `crates/core` | Tip states and transitions, `Sats`/`Msats`, typed IDs, `Clock` |
| `crates/crypto` | `Keyring` envelope encryption, `Redacted<T>`, webhook signature check, tokens |
| `crates/store` | Repositories, `tips::transition`, the job queue |
| `crates/runtime` | Config helpers, tracing, database pool, shutdown signal |
| `crates/worker` | `Runner` and job handlers |
| `crates/api` | Router, middleware, error contract, OpenAPI |
| `migrations/` | SQL migrations |
| `.sqlx/` | Offline query metadata for `sqlx` macros |
