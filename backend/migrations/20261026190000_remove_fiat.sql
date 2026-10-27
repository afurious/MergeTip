-- Fiat payouts are withdrawn (RFC 0004): receivers are paid over Lightning only. This contracts
-- the schema in one step instead of expand-then-contract because nothing using it was ever
-- released; no environment has fiat data.

DROP TABLE bitnob_events;
DROP TABLE payout_methods;

ALTER TABLE tips
    DROP COLUMN fiat_currency,
    DROP COLUMN fiat_amount,
    DROP COLUMN fx_rate,
    DROP COLUMN fees,
    DROP COLUMN provider_quote_id,
    DROP COLUMN provider_payout_id,
    DROP COLUMN provider_status;

ALTER TABLE tips DROP CONSTRAINT tips_state_check;
ALTER TABLE tips ADD CONSTRAINT tips_state_check CHECK (state IN (
    'received', 'awaiting_sender', 'verifying', 'awaiting_confirmation', 'held', 'paying',
    'pledged', 'paid', 'rejected', 'cancelled', 'expired', 'failed'));

DELETE FROM feature_flags WHERE key = 'fiat_payouts';
