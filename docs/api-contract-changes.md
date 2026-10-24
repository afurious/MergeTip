# API contract changes

Every change to the frozen v1 contract in [`api-contract.md`](api-contract.md), newest first.
Changes are additive: nothing is removed or renamed, and old shapes keep working.

| Date | Change | Why |
| --- | --- | --- |
| 2026-10-24 | `GET /healthz` documented in OpenAPI alongside `GET /v1/health` | Fly health checks already use `/healthz`; `/v1/health` is the contract endpoint for clients |
