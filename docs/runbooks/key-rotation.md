# Runbook: rotate `MASTER_KEY`

Every secret at rest (emails, GitHub user tokens, NWC connection strings, claim tokens, payout
beneficiaries) is envelope-encrypted: each value has its own data key, wrapped by `MASTER_KEY`.
Rotating re-wraps the small data keys; values themselves never leave memory in plaintext.
Rotate yearly, when someone with access leaves, or immediately if the key may have leaked (then
also follow [disable-payments.md](disable-payments.md)).

## Symptoms (unplanned rotation)

- The key appeared in a log, a commit, a screenshot or a compromised machine.
- `value opens with neither master key` in worker logs (a bad rotation; see step 6).

## Steps

1. Generate a new key: `openssl rand -hex 32`.
2. **Teach both apps the new key before either uses it.** Set it as the *previous* key, so both
   can open values sealed with it but neither seals with it yet:

   ```sh
   fly secrets set -a mergetip-api-production MASTER_KEY=<old> MASTER_KEY_PREVIOUS=<new>
   fly secrets set -a mergetip-worker-production MASTER_KEY=<old> MASTER_KEY_PREVIOUS=<new>
   ```

   Wait for both releases to finish (`fly status`).
3. **Swap**, on both apps:

   ```sh
   fly secrets set -a mergetip-api-production MASTER_KEY=<new> MASTER_KEY_PREVIOUS=<old>
   fly secrets set -a mergetip-worker-production MASTER_KEY=<new> MASTER_KEY_PREVIOUS=<old>
   ```

   Whichever app restarts first, the other can already open what it seals. Doing this in one
   step instead would leave a window where, say, the api seals a new wallet connection with a
   key the worker doesn't know, and that sender's payments fail.
4. Re-wrap everything (safe while the service runs, safe to repeat):

   ```sh
   fly ssh console -a mergetip-worker-production -C "/app ops rewrap --operator <your-name>"
   ```

   The log line `master key rewrap done` reports `checked`, `rewrapped` and `failed`.
5. Run it again: it must report `rewrapped=0` (anything written mid-run is caught by the
   second pass).
6. **Decide when to remove the old key.** Re-wrapping covers encrypted values, but not the
   values *signed* with keys derived from the master key: unsubscribe links in emails already
   sent and confirmation codes in flight. They verify only while `MASTER_KEY_PREVIOUS` holds
   the old key.
   - **Routine rotation:** keep the old key as `MASTER_KEY_PREVIOUS` until the next rotation, so
     old unsubscribe links keep working.
   - **Leaked key:** remove it now
     (`fly secrets unset -a mergetip-api-production MASTER_KEY_PREVIOUS`, and the worker).
     Unsubscribe links in emails sent before the rotation stop working; people can still turn
     emails off in Settings, and every new email carries a fresh link.
7. If `failed > 0`: those values open with neither key (sealed under a third key, or corrupt).
   The log names the column and row ID. Do **not** remove the previous key until each is
   understood; a failed NWC string only means that sender must reconnect their wallet.

## Verify recovery

- `rewrapped=0, failed=0` on the last run.
- After any key change: sign in on the web app (decrypts the GitHub token), `GET /v1/me`
  shows the email, and a test tip on staging pays (decrypts the NWC string).
