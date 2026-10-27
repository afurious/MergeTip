-- Which wallet connection made each payment attempt. Reconciliation must ask that wallet, not
-- whichever one the sender has connected since (M3 review). Null for attempts made before.
ALTER TABLE payment_attempts
    ADD COLUMN wallet_connection_id uuid REFERENCES wallet_connections (id);
