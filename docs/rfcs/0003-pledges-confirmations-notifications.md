# RFC 0003: Pledges, confirmations and notifications

- Author: backend
- PRD requirement(s): FR-4.4, FR-6.5, FR-11, FR-12, journeys B and C
- Status: accepted

## Problem

M3 pays the happy path. Every other path a tip can take waits on something: a merge, a sender's
wallet, a receiver's address, or the sender's confirmation. Each wait needs a deadline, a way out,
and a way to tell people. Timers fire twice, links get replayed and emails bounce, so each path
must be idempotent and must never move money on its own authority.

## Proposal

### Time windows

| State | Clock column | Window | On expiry | Who hears about it |
| --- | --- | --- | --- | --- |
| `awaiting_sender` | `expires_at` | 24 h from the comment | `cancelled` (`sender_timeout`) | Status line in the thread |
| `awaiting_confirmation` | `confirm_expires_at` | 1 h from verification | `cancelled` (`confirmation_timeout`) | Status line in the thread |
| `pledged` | `expires_at` | 30 days from the pledge | `expired` | Sender: new thread comment ("nothing was charged"). Receiver: final email |
| `pledged` reminder | `expires_at − 7 d` (day 23) | — | Reminder email | Receiver |
| `held` | none | until the PR closes | Merged: `paying`. Closed unmerged: `cancelled` (`pr_closed`) | Status line in the thread |

One recurring `expire_tips` job (dedupe key `expire_tips`, rescheduled every 60 s) handles every
window. It claims at most 500 due tips per state with `FOR UPDATE SKIP LOCKED`. Every move goes
through the compare-and-set in `store::tips::transition`, so two workers, or one job running
twice, cannot both move the same tip. Tests drive time with `FixedClock`.

### On-merge holds

`pull_request.closed` queues `resolve_held {installation, repo, number}`, deduplicated per PR.
The job fetches the PR from GitHub, because the API's answer is authoritative and the payload may
be stale.

- **Merged:** each `held` tip is verified again with the on-merge rules, because the PR's author
  or commits may have changed. Passing tips move `held → paying` and queue `pay_tip`. Failing tips
  move `held → cancelled` with the verifier's code.
- **Closed unmerged:** every `held` tip is cancelled (`pr_closed`).
- **Still open:** nothing happens.

Confirming an on-merge tip moves it to `held` and queues the same job. A PR that merged before
the confirmation is therefore paid at once, without the API needing a GitHub installation token.

### Awaiting sender

The connect-wallet comment and the 24 h `expires_at` are unchanged from M2. `PUT /me/wallet`
queues `resume_sender_tips {user_id}` in the same transaction that stores the connection. That
job moves every unexpired `awaiting_sender` tip of the user to `verifying` and queues
`verify_tip`. A tip whose window has passed is left for the expiry job, so it can't be revived
after its deadline.

### Pledges and claim tokens

`pay_tip` makes a pledge when the receiver has no Lightning address:

1. `paying → pledged`, with `expires_at = now + 30 d`.
2. A claim token is minted with `crypto::random_token`.
3. The token's SHA-256 is stored in `tips.claim_token_hash` (the lookup key, unique).
4. The raw token is stored envelope-encrypted in `tips.claim_token_ciphertext`, with AAD
   `tips.claim_token:{id}`.
5. The pledge comment and a `notify_pledge` job are queued in the same transaction.

Why the token is also stored encrypted: the reminder (day 23) and expiry emails must repeat the
link long after the first comment, and a hash cannot be reversed. This adds no real exposure.
The raw link is already public in the GitHub thread, because the PRD's pledge reply contains it.
The security boundary is the receiver's GitHub sign-in (FR-11.6), not the link's secrecy.

Lifecycle:

| Step | Effect |
| --- | --- |
| Minted | Hash and ciphertext stored. The link appears in the thread and in emails |
| `GET /claims/{token}` | Public, read-only, minimal tip. 404 if no tip has the hash. 410 if expired |
| `POST /claims/{token}` | Session user must be the tip's receiver (`receiver_id`, linked by `github_id` at sign-in), otherwise 403. The tip is locked; 409 if not `pledged`, 410 if past `expires_at`. Address checks run before the lock |
| Claimed | In one transaction: both token columns are nulled (single use), `pledged → paying`, `pay_tip` queued. Reusing the token now gives 404 |
| Expired | `pledged → expired`. The token stops working (410) |

### High-value confirmation and OTP

`verifying → awaiting_confirmation` is unchanged from M2. Only the web endpoints can move a tip out
of that state. They require a `SessionUser` who is the tip's sender, so the extension's bearer
token can't do it. The comment handler has no code path that confirms anything.

- **`POST /confirmations/{id}/otp`** (only when `confirm_otp_enabled`):
  - draws a 6-digit code from the CSPRNG;
  - stores `hex(HMAC-SHA256(subkey("otp"), "{tip_id}:{code}"))` in `otp_codes`, one row per tip,
    expiring after 10 min, with an attempt counter;
  - emails the code synchronously from the API;
  - allows at most one code per 60 s per tip (429 `otp_rate_limited`).

  The code never reaches the jobs table or the logs.
- **`POST /confirmations/{id}`**:
  1. Lock the tip.
  2. Check it is `awaiting_confirmation`, else 409. Check it hasn't expired, else 410.
  3. If OTP is on: count the attempt and commit the count before comparing, so failures can't be
     rolled back. Compare in constant time against every verification subkey. The 5th wrong
     attempt deletes the code. A wrong code gives 422 `otp_invalid`.
  4. Set `confirmed_at`.
  5. A `now` tip moves to `paying` with `pay_tip` queued. An `on_merge` tip moves to `held` with
     `resolve_held` queued.
- **`DELETE /confirmations/{id}`**: the tip moves to `cancelled` (`cancelled_by_sender`).

### Email

**Delivery.** The `notify` crate provides a `Mailer` trait with two implementations:

- `ResendMailer`, used when `RESEND_API_KEY` is set. It sends an `Idempotency-Key` header and has
  a 10 s timeout.
- `SmtpMailer` (lettre), used when `SMTP_URL` is set, for Mailpit in dev.

Templates are askama, with HTML and text parts. Every message carries `List-Unsubscribe`
(an API URL with a signed token) and `List-Unsubscribe-Post: List-Unsubscribe=One-Click`
(RFC 8058).

**Source chain** (FR-11.2), run by `notify_pledge` in the worker, first match wins:

1. The receiver's verified email, if they have signed in. It is already stored.
2. The `email` on their public profile (`GET /users/{login}`).
3. The author email of the receiver's commit on the PR. This step runs only when commit emails
   are allowed (see the privacy note below).

Any `*@users.noreply.github.com` address is skipped. A found address is stored encrypted with its
`email_source`, and a weaker source never overwrites a stronger one. If none is found, the tip
gets `email_status = none`.

**Privacy note (PRD open question).** Commit emails are public in git history, but the user never
gave them to MergeTip. Whether emailing them is acceptable under GitHub's terms and our privacy
policy is undecided. The feature flag `email_from_commits` controls it. With no row present, the
default is on outside production and off in production. An explicit row always wins, so the
privacy decision is a one-row change. Every email offers one-click unsubscribe.

**Digest rules** (FR-11.4):

- Every reason to email a receiver (a new pledge, a reminder coming due, an expiry) queues
  `send_digest {user_id}`, with `run_at = max(now, users.last_email_sent_at + 24 h)` and
  `dedupe_key = "digest:{user_id}:{run_at date}"`.
- While one digest is queued, later reasons fold into it. Three pledges in a day become one
  email.
- When it runs, the digest lists every open pledge, flags each as new, expiring soon or expired,
  and sets `last_notified_at` on each tip it covered.
- A tip is due:
  - **new**: never notified;
  - **reminder**: `now ≥ expires_at − 7 d` and the last notice was before that point;
  - **final**: expired, and the last notice was before expiry.
- No due tips means no email.
- Opted-out users and users with no address get no email. Their due tips are still marked
  notified, so the expiry job doesn't queue them again every minute.
- A single due item uses its own template (pledge, reminder or expiry). Several use the digest
  template.

**Unsubscribe.** The token is `{user_id}.{base64url(HMAC-SHA256(subkey("unsubscribe"),
"unsubscribe:{user_id}"))}`. It needs no table, survives master-key rotation through
`verification_subkeys`, and is checked in constant time. `GET` and `POST
/v1/email/unsubscribe?token=` set `email_opt_out`. GET also unsubscribes, because people click
links from mail clients that can't POST. A link scanner that prefetches it costs only email
notices; GitHub mentions continue.

**Delivery tracking.** `POST /v1/webhooks/resend` handles Resend's delivery events:

1. Verify the Svix signature: HMAC-SHA256 over `"{svix-id}.{svix-timestamp}.{body}"` with the
   base64 secret after `whsec_`. Check every `v1,` signature in constant time, and reject
   timestamps more than 5 min away.
2. Map `email.sent`, `email.delivered`, `email.opened` and `email.bounced` onto the
   `tips.email_status` of every tip the message covered. The `email_messages` table records
   which tips each message covered.
3. Only move forward: `pending < sent < delivered < opened`. `bounced` and `failed` are final.

## Data model / API changes

Migration `edge_flows`, additive only:

- `tips.claim_token_ciphertext bytea`, and `opened` added to the `email_status` check.
- `users.last_email_sent_at timestamptz`.
- `otp_codes (tip_id pk → tips, code_hmac, expires_at, attempts, created_at)`.
- `email_messages (provider_id pk, user_id, tip_ids uuid[], sent_at)`.

API additions beyond the frozen contract are logged in `docs/api-contract-changes.md`:

- `GET` and `POST /v1/email/unsubscribe`, and `POST /v1/webhooks/resend`;
- error codes `not_sender`, `otp_rate_limited`, `otp_unavailable`, `not_awaiting_confirmation`
  and `github_forbidden`.

Frontend heads-up: the claim page route must be `/claim/[token]` (the scaffold has
`claim/[tipId]`).

## Security and payment-safety impact

- Confirmation needs a web session and, optionally, an emailed code. A hijacked GitHub account
  can only *ask* for a large tip.
- Residual risk: whoever holds the web session can turn OTP off in settings. Re-authenticating to
  change security settings is an M5 follow-up.
- Claims need the receiver's own GitHub sign-in, and are single use and time-boxed. Nothing is
  charged until a claim succeeds.
- Every timer runs through the compare-and-set transition, so a timer firing twice is a no-op.
- OTPs are HMACed under an HKDF subkey (6 digits hash too cheaply to brute-force a plain hash).
  They allow 5 attempts, last 10 min, are rate-limited, and are never logged.
- Emails, tokens and OTPs never reach the logs. Email addresses are encrypted at rest with
  per-row AAD.

## Alternatives considered

- **A deterministic HMAC claim token instead of storing a ciphertext.** Rejected: rotating the
  master key would silently invalidate every live link, and it buys no secrecy the public thread
  doesn't already give away.
- **Queuing the OTP email as a job.** Rejected: the plaintext code would sit in `jobs.payload`.
- **A `merged_pulls` table so the API can confirm an on-merge tip straight to `paying`.**
  Rejected: it duplicates GitHub's state, and `resolve_held` already covers the case.

## Rollout (feature flag, migration plan)

- Additive migration. `email_from_commits` governs commit emails as described above.
- Email sending is off unless `RESEND_API_KEY` or `SMTP_URL` is set. Pledges still work, notified
  by mention only.
