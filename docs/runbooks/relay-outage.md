# Runbook: Nostr relay outage (NWC)

Senders' wallets are reached over Nostr relays named in their NWC connection strings. MergeTip
runs no relay of its own in production; when a popular relay (or a wallet's own relay) is down,
payments to those wallets time out.

## Symptoms

- `NwcTimeoutsHigh` (> 10% of payments timing out over 15 min).
- `nwc_errors_total{code=~"TIMEOUT|RELAY"}` rising; tips piling up in `paying` with attempts in
  `unknown`.

## Check

1. Which relays: `SELECT relays[1], count(*) FROM wallet_connections WHERE revoked_at IS NULL GROUP BY 1 ORDER BY 2 DESC;`
   and the same joined to tips currently `paying`.
2. Is the relay up? `websocat <relay url>` or a relay status page (e.g. nostr.watch).

## Steps

1. Nothing must be done for safety: timed-out payments are reconciled with `lookup_invoice`
   and never retried blindly (RFC 0002). They resolve when the relay returns or the invoice
   expires.
2. If one relay is down for hours, tell affected senders (bot reply or email) to reconnect a
   wallet on a different relay in Settings → Wallet.
3. If timeouts are spread across all relays, suspect our egress: check the worker machines'
   network (`fly ssh console`, `curl -I https://example.com`).

## Verify recovery

`nwc_errors_total{code="TIMEOUT"}` rate drops; `SELECT count(*) FROM payment_attempts WHERE status = 'unknown'` drains.
