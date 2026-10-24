# API contract

The REST API shared by the backend (producer) and the web app and extension (consumers).

- **Machine-readable source of truth:** [`openapi/openapi.json`](../openapi/openapi.json), generated
  from the Rust code with `make openapi`. It documents the routes implemented so far.
- **This page:** the agreed shape of the whole v1 API, including routes still to be built. The
  "Milestone" column says when each route lands.
- **Changes:** additive only, each logged in [`api-contract-changes.md`](api-contract-changes.md).

## Contract

The base path is `/v1` and everything is JSON. CORS allows `PUBLIC_BASE_URL` (the web app) with credentials. Errors are `{"error":{"code":"snake_case","message":"Human text"}}` with the matching HTTP status.

- **Web auth:** the cookie `mt_session`, set with `HttpOnly; Secure` (not `Secure` in dev)`; SameSite=Lax; Path=/`.
- **Extension auth:** `Authorization: Bearer <token>`, valid for 7 days.
- **Auth column meanings:** `any` means cookie or bearer, and `session` means cookie only.

```ts
type TipState = "received" | "awaiting_sender" | "verifying" | "awaiting_confirmation"
  | "held" | "paying" | "paid" | "pledged" | "rejected" | "cancelled" | "expired" | "failed";
type UserRef = { login: string; avatar_url: string };
type Tip = {
  id: string; state: TipState; amount_sats: number; kind: "now" | "on_merge";
  note: string | null; sender: UserRef; receiver: UserRef;
  repo: string; thread_number: number; thread_url: string;
  payment_hash: string | null; created_at: string; updated_at: string; expires_at: string | null;
};
type Me = {
  id: string; github_login: string; avatar_url: string; email: string | null;
  lightning_address: string | null;
  wallet: { connected: boolean; relay: string | null; methods: string[] };
  caps: { per_tip_max_sats: number; daily_max_sats: number; spent_today_sats: number };
  confirm_threshold_sats: number; confirm_otp_enabled: boolean; email_opt_out: boolean;
};
```

| Method | Path | Auth | Body | Response | Milestone |
| --- | --- | --- | --- | --- | --- |
| GET | `/auth/github/login?redirect=<path>` | — | — | 302 to GitHub | M3 |
| GET | `/auth/github/callback` | — | — | Sets cookie; 302 to `PUBLIC_BASE_URL + redirect` (default `/dashboard`) | M3 |
| POST | `/auth/logout` | session | — | 204 | M3 |
| POST | `/auth/extension-token` | session | — | `{ token, expires_at }` | M3 |
| GET | `/me` | any | — | `Me` · 401 `unauthenticated` | M3 |
| PUT | `/me/wallet` | any | `{ nwc_uri }` | `Me` · 422 `nwc_invalid_uri`, `nwc_unreachable`, `nwc_missing_pay_invoice` | M3 |
| DELETE | `/me/wallet` | any | — | 204 | M3 |
| PUT | `/me/lightning-address` | any | `{ address }` | `Me` · 422 `lnurl_unreachable`, `lnurl_invalid` | M3 |
| PATCH | `/me/settings` | any | `{ per_tip_max_sats?, daily_max_sats?, confirm_threshold_sats?, confirm_otp_enabled?, email_opt_out? }` | `Me` · 422 `invalid_setting` | M3 |
| DELETE | `/me` | session | — | 204 (anonymise tips, delete secrets) | M4 |
| GET | `/me/tips?direction=sent\|received\|all&state=&cursor=` | any | — | `{ items: Tip[], next_cursor: string \| null }` | M4 |
| GET | `/claims/{token}` | — | — | `{ tip: Tip }` · 404 `claim_not_found`, 410 `claim_expired` | M4 |
| POST | `/claims/{token}` | session (receiver) | `{ lightning_address? }` | `{ tip: Tip }` · 403 `not_receiver`, 409 `already_claimed`, 422 `no_address` | M4 |
| GET | `/confirmations/{tip_id}` | session (sender) | — | `{ tip: Tip }` | M4 |
| POST | `/confirmations/{tip_id}/otp` | session (sender) | — | 204 (emails a 6-digit code) | M4 |
| POST | `/confirmations/{tip_id}` | session (sender) | `{ otp?: string }` | `{ tip: Tip }` · 410 `confirmation_expired`, 422 `otp_invalid` | M4 |
| DELETE | `/confirmations/{tip_id}` | session (sender) | — | 204 (cancels the tip) | M4 |
| GET | `/users/{login}/receive-status` | any | — | `{ login, method: "lightning_address" \| "none" }` | M4 |
| POST | `/tips/comment` | any | `{ repo, thread_number, receiver, amount_sats, kind, note? }` | 202 `{ comment_url }` · 403 `app_not_installed` | M4 |
| GET | `/health` | — | — | `{ status: "ok", version }` | M1 |
| POST | `/webhooks/github` | HMAC | raw | 202 | M2 |

`/healthz` (outside `/v1`, plain `ok`) stays as-is for the Fly health checks.

Links in bot messages and emails:

- Claims: `${PUBLIC_BASE_URL}/claim/{token}`
- Confirmations: `${PUBLIC_BASE_URL}/confirm/{tip_id}`
- Wallet connect: `${PUBLIC_BASE_URL}/settings/wallet`

**Changing the contract:** you may add endpoints, fields and enum values. Removing or renaming anything is not allowed. Log every change in [`api-contract-changes.md`](api-contract-changes.md) with the date, what changed and why. Then run `make openapi` to regenerate `openapi/openapi.json` and the TypeScript client, and keep the old shape working.
