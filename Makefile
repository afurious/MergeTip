# MergeTip developer tasks. Run `make help` for the list.
SHELL := bash
BACKEND := backend
FRONTEND := frontend

# Load local settings (DATABASE_URL etc.) when present; the file is git-ignored.
-include .env
export DATABASE_URL SMEE_URL

.PHONY: help bootstrap up down dev dev-api dev-worker dev-web dev-app smee migrate migrate-new \
        sqlx-prepare openapi fmt lint test check check-backend coverage clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(firstword $(MAKEFILE_LIST)) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

bootstrap: ## Install toolchains and dependencies
	rustup show active-toolchain
	cargo install --locked sqlx-cli --no-default-features --features rustls,postgres
	cargo install --locked cargo-deny cargo-audit cargo-watch
	corepack enable
	cd $(FRONTEND) && pnpm install
	@[ -f .env ] || cp .env.example .env

up: ## Start local services (Postgres, relay, mocks, Mailpit)
	docker compose up -d

down: ## Stop local services
	docker compose down

dev: ## Run api, worker and web app with hot reload
	$(MAKE) -j3 dev-api dev-worker dev-web

dev-api:
	cd $(BACKEND) && cargo watch -x 'run -p mergetip-api'

dev-worker:
	cd $(BACKEND) && cargo watch -x 'run -p mergetip-worker'

dev-web:
	cd $(FRONTEND) && pnpm dev

dev-app: ## Create a personal GitHub App from deploy/manifests/dev.json (needs SMEE_URL)
	cd $(BACKEND) && cargo run --quiet -p dev-app --

smee: ## Forward GitHub webhooks to the local api
	npx smee-client --url $$SMEE_URL --target http://localhost:8080/v1/webhooks/github

migrate: ## Apply database migrations
	cd $(BACKEND) && sqlx migrate run

migrate-new: ## Create a migration: make migrate-new name=add_things
	@[ -n "$(name)" ] || { echo "usage: make migrate-new name=<snake_case_name>"; exit 1; }
	cd $(BACKEND) && sqlx migrate add $(name)

sqlx-prepare: ## Refresh backend/.sqlx offline query data (needs DATABASE_URL)
	cd $(BACKEND) && cargo sqlx prepare --workspace -- --all-targets

openapi: ## Regenerate openapi/openapi.json and the TS client
	@# Build into a temp file first: a failed build must not truncate the committed spec.
	cd $(BACKEND) && cargo run --quiet -p mergetip-api -- openapi > ../openapi/openapi.json.tmp \
		&& mv ../openapi/openapi.json.tmp ../openapi/openapi.json \
		|| { rm -f ../openapi/openapi.json.tmp; exit 1; }
	cd $(FRONTEND) && pnpm gen:api

fmt: ## Format all code
	cd $(BACKEND) && cargo fmt --all
	cd $(FRONTEND) && pnpm format

lint: ## Lint all code
	cd $(BACKEND) && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
	cd $(FRONTEND) && pnpm lint

check: ## Type-check the frontend
	cd $(FRONTEND) && pnpm check

check-backend: ## Backend gate: fmt, clippy, tests, sqlx offline data (needs DATABASE_URL)
	cd $(BACKEND) && cargo fmt --all --check
	cd $(BACKEND) && cargo clippy --workspace --all-targets -- -D warnings
	cd $(BACKEND) && cargo test --workspace
	cd $(BACKEND) && cargo sqlx prepare --workspace --check -- --all-targets

coverage: ## Line coverage per crate, checked against the CI floors (needs DATABASE_URL)
	cd $(BACKEND) && cargo llvm-cov --workspace --all-features --no-report
	cd $(BACKEND) && cargo llvm-cov report --json --summary-only | python3 scripts/coverage_gate.py

test: ## Run all tests
	cd $(BACKEND) && cargo test --workspace
	cd $(FRONTEND) && pnpm test

clean: ## Remove build artifacts
	cd $(BACKEND) && cargo clean
	rm -rf $(FRONTEND)/node_modules $(FRONTEND)/**/build $(FRONTEND)/**/.svelte-kit $(FRONTEND)/**/.output
