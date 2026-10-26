# Multi-stage build for the backend binaries, with dependency caching (cargo-chef).
#   docker build --build-arg BIN=mergetip-api \
#     --build-arg VERSION=$(git describe --tags --always) --build-arg GIT_SHA=$(git rev-parse HEAD) \
#     -t mergetip-api backend/
# BIN is one of: mergetip-api, mergetip-worker, mock-nwc, mock-lnurl.
ARG BIN=mergetip-api

FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.78
WORKDIR /src

# Only the dependency graph: this layer changes when Cargo.toml/Cargo.lock do, not on every edit.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
ARG BIN
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json --bin ${BIN}
COPY . .
# Queries are checked against the committed .sqlx data; no database at build time.
ENV SQLX_OFFLINE=true
RUN cargo build --release --locked --bin ${BIN} && cp target/release/${BIN} /app

FROM gcr.io/distroless/cc-debian12:nonroot
ARG BIN
ARG VERSION=dev
ARG GIT_SHA=unknown
LABEL org.opencontainers.image.title="${BIN}" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${GIT_SHA}" \
      org.opencontainers.image.source="https://github.com/Xoulomon/MergeTip" \
      org.opencontainers.image.licenses="MIT"
COPY --from=build /app /app
USER nonroot:nonroot
ENTRYPOINT ["/app"]
