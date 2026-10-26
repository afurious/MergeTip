# API contract changes

Every change to the frozen v1 contract in [`api-contract.md`](api-contract.md), newest first.
Changes are additive: nothing is removed or renamed, and old shapes keep working.

| Date | Change | Why |
| --- | --- | --- |
| 2026-10-24 | GitHub App manifests: the OAuth callback is `${API_PUBLIC_URL}/v1/auth/github/callback` (was the web app's `/auth/github/callback`), and the App requests `email_addresses: read` | The API exchanges the code and sets the session cookie, as the contract's `/auth/github/callback` route says; the email permission provides the verified email for pledge notices (FR-11.2). Infrastructure only: no API shape changes |
| 2026-10-24 | `GET /healthz` documented in OpenAPI alongside `GET /v1/health` | Fly health checks already use `/healthz`; `/v1/health` is the contract endpoint for clients |
