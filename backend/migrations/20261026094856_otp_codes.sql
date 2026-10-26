-- One-time confirmation codes for high-value tips (FR-12.5, RFC 0003). One live code per tip.
-- Only an HMAC of the code is stored (a plain hash of 6 digits is trivially reversed).
CREATE TABLE otp_codes (
    tip_id     uuid PRIMARY KEY REFERENCES tips (id) ON DELETE CASCADE,
    code_hmac  text NOT NULL,
    expires_at timestamptz NOT NULL,
    attempts   integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    created_at timestamptz NOT NULL DEFAULT now()
);
