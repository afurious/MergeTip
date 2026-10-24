# ⚡ MergeTip

Tip open-source contributors in sats, straight from GitHub, for verified work: a review, a merged
PR, triage or docs. Payments are non-custodial: senders pay from their own wallet over Nostr Wallet
Connect (NWC), and receivers get paid to their own Lightning address.

```
/tip @reviewer 2100 for the review
/tip @newdev 5k on-merge
```

## Repository layout

```
.
├── backend/                 # Rust (Cargo workspace)
│   ├── crates/
│   │   ├── api/             # axum HTTP server: routes, auth, webhook endpoints
│   │   ├── worker/          # job runner: payments, retries, expiry, emails
│   │   ├── core/            # domain types, tip state machine, errors
│   │   ├── parser/          # /tip command grammar (pure, no I/O)
│   │   ├── github/          # octocrab wrapper: App auth, verifier, bot replies
│   │   ├── payer/           # NWC client, LNURL resolver, invoice validation
│   │   ├── fiat/            # Bitnob client: quotes, payouts, webhooks
│   │   ├── notify/          # email (Resend/Postmark) + templates
│   │   ├── store/           # sqlx repositories, job queue
│   │   ├── crypto/          # envelope encryption, HMAC helpers, Redacted<T>
│   │   └── runtime/         # config, tracing, database pool, shutdown (shared by binaries)
│   ├── devtools/            # mock NWC wallet + mock LNURL server for local dev and tests
│   ├── migrations/          # SQL migrations (sqlx)
│   └── Dockerfile           # builds any backend binary (BIN build arg)
├── frontend/                # TypeScript (pnpm workspace)
│   ├── apps/
│   │   ├── web/             # SvelteKit web app: landing, claim, confirm, dashboard, settings
│   │   └── extension/       # WXT Manifest V3 extension (Chrome, Firefox)
│   └── packages/
│       ├── ui/              # shared Svelte components, Primer-style tokens
│       └── api-client/      # TypeScript client generated from openapi/openapi.json
├── openapi/                 # API contract shared by backend (producer) and frontend (consumer)
├── deploy/                  # Fly.io configs, GitHub App manifests per environment
├── docs/                    # ADRs, RFCs, runbooks
├── docker-compose.yml       # Postgres, Nostr relay, Mailpit, mock wallet + LNURL server
├── Makefile                 # cross-project tasks (`make help`)
└── .github/                 # CI, PR and issue templates
```

## Tech stack

| Layer | Choice |
| --- | --- |
| Backend | Rust, tokio, axum, tower-http |
| Database | Postgres 16 via sqlx; `FOR UPDATE SKIP LOCKED` job queue |
| GitHub | octocrab (App JWT + installation tokens) |
| Payments | rust-nostr `nwc` (NIP-47/NIP-44), `lnurl-rs`, `lightning-invoice` |
| Fiat | Bitnob Payouts API (NG, GH, KE) |
| Email | Resend (fallback Postmark); Mailpit locally |
| API contract | OpenAPI via utoipa → openapi-typescript + openapi-fetch |
| Web app | SvelteKit, Svelte 5, Tailwind CSS, Breez SDK (Spark) WASM |
| Extension | WXT, Svelte, Manifest V3 |
| Hosting | Fly.io, Cloudflare |

## Getting started

Prerequisites: Rust (stable), Node 22+, pnpm, Docker.

```sh
make bootstrap      # toolchains, sqlx-cli, frontend deps, .env
make up             # Postgres, relay, Mailpit, mocks
make migrate        # apply migrations
make dev            # api :8080, worker, web :5173
```

Other tasks: `make help`. Backend details: [`docs/backend/README.md`](docs/backend/README.md).

Working on one side only:

```sh
cd backend  && cargo test --workspace
cd frontend && pnpm dev          # web app
cd frontend && pnpm dev:ext      # browser extension
```

## License

[MIT](LICENSE)
