# Backend

Rust services for MergeTip. See the root `README.md` for the repository layout.

## Quick start

```sh
make up                  # Postgres and the local mocks (set POSTGRES_PORT if 5432 is taken)
make migrate             # apply migrations
make check-backend       # fmt, clippy, tests, sqlx offline data check
```
