-- Claim links (RFC 0003). `claim_token_hash` (already present) is the lookup key; the raw token
-- is also kept envelope-encrypted (AAD `tips.claim_token:{id}`) because reminder and expiry
-- emails repeat the link weeks later. The link is public in the GitHub thread anyway: claiming
-- is protected by the receiver's GitHub sign-in, not by the link's secrecy.
ALTER TABLE tips ADD COLUMN claim_token_ciphertext bytea;
