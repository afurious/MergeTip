# Contributing

- **Branching:** trunk-based on `main`; short-lived feature branches; squash merges.
- **Reviews:** every PR needs one approval and green CI.
- **Commits:** [Conventional Commits](https://www.conventionalcommits.org/) (`feat(payer): …`, `fix(web): …`).
- **Design notes:** any change touching payments, auth or the data model needs a one-page RFC in
  `docs/rfcs/` reviewed before coding.
- **Feature flags:** risky features (extension tip button, auto-tips) ship behind
  flags stored in Postgres.
- **Migrations:** expand–contract only; rollback must never need a down-migration.

Before pushing:

```sh
make fmt lint test
```
