# Migrations

SQL migrations managed by `sqlx-cli` and embedded in the binaries via `mergetip_store::MIGRATOR`.
Create one from the repository root with:

```sh
make migrate-new name=<snake_case_name>
```

Rules:

- **Expand only.** Add tables, columns and indexes; drop old ones in a later release once no
  running code reads them. Rollback is "redeploy the previous image", never a down-migration, so
  migrations are not reversible.
- **Never edit a committed migration.** Its checksum is recorded in `_sqlx_migrations`.
- No locking migrations on `tips` during business hours (`CREATE INDEX CONCURRENTLY` in its own
  migration file).
- After changing any query or migration, run `make sqlx-prepare` and commit `backend/.sqlx/`.
