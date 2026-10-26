# API contract changes

Every change to the frozen v1 contract in [`api-contract.md`](api-contract.md), newest first.
Changes are additive: nothing is removed or renamed, and old shapes keep working.

| Date | Change | Why |
| --- | --- | --- |
| 2026-10-26 | Added `GET` and `POST /v1/email/unsubscribe?token=` (GET answers with an HTML page; POST is RFC 8058 one-click and returns 204; 400 `invalid_token`) and `POST /v1/webhooks/resend` (Svix-signed Resend delivery events; 401 `invalid_signature`, 404 when unconfigured) | One-click unsubscribe in every email (FR-11.5) and delivered/bounced/opened tracking (FR-11.7). Neither is called by the web app |
| 2026-10-26 | Confirmation error codes added: `not_awaiting_confirmation` (409, tip already confirmed, cancelled or expired), `otp_not_enabled` (422, codes are off), `otp_unavailable` (422, no email on file or email not configured), `otp_rate_limited` (429, one code a minute), `email_failed` (502). `POST /v1/confirmations/{tip_id}` accepts an empty body when codes are off | The frozen contract names only the happy path, `confirmation_expired` and `otp_invalid`; clients need to tell the other outcomes apart |
| 2026-10-26 | Frontend heads-up, no API change: claim links are `${PUBLIC_BASE_URL}/claim/{token}`, so the web route must be `/claim/[token]` (the scaffold has `claim/[tipId]`). `GET`/`POST /v1/claims/{token}` take that token, not a tip ID | Claim links carry an unguessable single-use token (FR-11.6); a tip ID would let anyone enumerate pledges |
| 2026-10-26 | `POST /v1/claims/{token}` accepts an empty body (no `Content-Type`) as well as `{ lightning_address? }` | "Claim with my saved address" needs no body |
| 2026-10-24 | GitHub App manifests: the OAuth callback is `${API_PUBLIC_URL}/v1/auth/github/callback` (was the web app's `/auth/github/callback`), and the App requests `email_addresses: read` | The API exchanges the code and sets the session cookie, as the contract's `/auth/github/callback` route says; the email permission provides the verified email for pledge notices (FR-11.2). Infrastructure only: no API shape changes |
| 2026-10-24 | `GET /healthz` documented in OpenAPI alongside `GET /v1/health` | Fly health checks already use `/healthz`; `/v1/health` is the contract endpoint for clients |
