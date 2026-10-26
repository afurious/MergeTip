-- Pledge emails (FR-11, RFC 0003 "Email").

-- When the user was last emailed: digests wait until 24 h after it.
ALTER TABLE users ADD COLUMN last_email_sent_at timestamptz;

-- Delivery tracking can now record opens. Widening a CHECK is additive.
ALTER TABLE tips DROP CONSTRAINT tips_email_status_check;
ALTER TABLE tips ADD CONSTRAINT tips_email_status_check CHECK (email_status IN (
    'none', 'pending', 'sent', 'delivered', 'opened', 'bounced', 'failed'));

-- Which tips each sent message covered, so provider webhooks (keyed by message ID) can update
-- every tip in a digest.
CREATE TABLE email_messages (
    provider_id text PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users (id),
    tip_ids     uuid[] NOT NULL,
    sent_at     timestamptz NOT NULL DEFAULT now()
);

-- Finding a receiver's pledges for a digest.
CREATE INDEX tips_receiver_notice_idx ON tips (receiver_id, state, last_notified_at);
