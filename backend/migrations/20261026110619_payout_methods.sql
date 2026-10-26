-- Fiat payouts through Bitnob (RFC 0004). Additive only.

-- One row per fiat destination. The Lightning address stays in users.lightning_address and is
-- the default when no fiat method is marked default.
CREATE TABLE payout_methods (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id                uuid NOT NULL REFERENCES users (id),
    kind                   text NOT NULL CHECK (kind IN ('lightning_address', 'bitnob_fiat')),
    country                text,
    currency               text,
    destination_type       text CHECK (destination_type IN ('bank', 'mobile_money')),
    -- Envelope-encrypted beneficiary JSON, AAD `payout_methods.beneficiary:{id}`.
    beneficiary_ciphertext bytea,
    provider_customer_id   text,
    verified_at            timestamptz,
    is_default             boolean NOT NULL DEFAULT false,
    created_at             timestamptz NOT NULL DEFAULT now(),
    CHECK (kind <> 'bitnob_fiat' OR (country IS NOT NULL AND currency IS NOT NULL
                                     AND destination_type IS NOT NULL
                                     AND beneficiary_ciphertext IS NOT NULL))
);
CREATE INDEX payout_methods_user_idx ON payout_methods (user_id);
CREATE UNIQUE INDEX payout_methods_one_default_idx ON payout_methods (user_id) WHERE is_default;

-- What a fiat tip was quoted and paid out at. Amounts are NUMERIC, never floats.
ALTER TABLE tips
    ADD COLUMN fiat_currency      text,
    ADD COLUMN fiat_amount        numeric,
    ADD COLUMN fx_rate            numeric,
    ADD COLUMN fees               numeric,
    ADD COLUMN provider_quote_id  text,
    ADD COLUMN provider_payout_id text UNIQUE,
    ADD COLUMN provider_status    text;

-- Bitnob webhook deliveries; the primary key deduplicates retries.
CREATE TABLE bitnob_events (
    event_id     text PRIMARY KEY,
    payload      jsonb NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz
);
