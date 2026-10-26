-- Sign-in sessions (RFC 0002). Only a SHA-256 of the token is stored.
CREATE TABLE sessions (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind         text NOT NULL CHECK (kind IN ('web', 'extension')),
    token_hash   text NOT NULL UNIQUE,
    expires_at   timestamptz NOT NULL,
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    user_agent   text,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sessions_user_idx ON sessions (user_id);

-- One row per attempt to pay a tip's invoice, written before the wallet is asked to pay.
CREATE TABLE payment_attempts (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tip_id             uuid NOT NULL REFERENCES tips (id),
    payment_hash       text NOT NULL,
    bolt11             text NOT NULL,
    amount_msat        bigint NOT NULL CHECK (amount_msat > 0),
    invoice_expires_at timestamptz NOT NULL,
    status             text NOT NULL CHECK (status IN ('pending', 'succeeded', 'failed', 'unknown')),
    nwc_error_code     text,
    started_at         timestamptz NOT NULL DEFAULT now(),
    finished_at        timestamptz
);
CREATE INDEX payment_attempts_tip_idx ON payment_attempts (tip_id);
CREATE INDEX payment_attempts_started_idx ON payment_attempts (started_at);
-- Double-pay guards: never two payments in flight for one tip, never two that succeeded.
CREATE UNIQUE INDEX payment_attempts_one_open_idx ON payment_attempts (tip_id)
    WHERE status IN ('pending', 'unknown');
CREATE UNIQUE INDEX payment_attempts_one_success_idx ON payment_attempts (tip_id)
    WHERE status = 'succeeded';
