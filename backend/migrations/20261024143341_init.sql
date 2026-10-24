-- Initial schema (RFC 0001). Enumerations are TEXT + CHECK so new values are additive.

CREATE FUNCTION set_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

-- One row per GitHub account, sender or receiver.
CREATE TABLE users (
    id                              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    github_id                       bigint NOT NULL UNIQUE,
    github_login                    text NOT NULL,
    avatar_url                      text,
    github_created_at               timestamptz,
    lightning_address               text,
    per_tip_max_sats                bigint NOT NULL DEFAULT 10000 CHECK (per_tip_max_sats > 0),
    daily_max_sats                  bigint NOT NULL DEFAULT 50000 CHECK (daily_max_sats > 0),
    confirm_threshold_sats          bigint NOT NULL DEFAULT 50000
                                        CHECK (confirm_threshold_sats >= 1000),
    confirm_otp_enabled             boolean NOT NULL DEFAULT false,
    email_ciphertext                bytea,
    email_source                    text CHECK (email_source IN ('verified', 'public_profile', 'commit')),
    email_opt_out                   boolean NOT NULL DEFAULT false,
    github_token_ciphertext         bytea,
    github_refresh_token_ciphertext bytea,
    github_token_expires_at         timestamptz,
    created_at                      timestamptz NOT NULL DEFAULT now(),
    updated_at                      timestamptz NOT NULL DEFAULT now(),
    deleted_at                      timestamptz
);
CREATE INDEX users_github_login_idx ON users (lower(github_login));
CREATE TRIGGER users_set_updated_at BEFORE UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE installations (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    github_installation_id bigint NOT NULL UNIQUE,
    account_login          text NOT NULL,
    suspended_at           timestamptz,
    config_yaml            text,
    config_fetched_at      timestamptz,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER installations_set_updated_at BEFORE UPDATE ON installations
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE installation_repos (
    installation_id uuid NOT NULL REFERENCES installations (id) ON DELETE CASCADE,
    github_repo_id  bigint NOT NULL,
    full_name       text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (installation_id, github_repo_id)
);
CREATE INDEX installation_repos_full_name_idx ON installation_repos (lower(full_name));

-- Raw webhook deliveries; the primary key deduplicates redeliveries of the same delivery.
CREATE TABLE github_events (
    delivery_id  text PRIMARY KEY,
    event        text NOT NULL,
    action       text,
    payload      jsonb NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz
);

CREATE TABLE wallet_connections (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id            uuid NOT NULL REFERENCES users (id),
    nwc_uri_ciphertext bytea NOT NULL,
    wallet_pubkey      text NOT NULL,
    relays             text[] NOT NULL,
    methods            text[] NOT NULL,
    encryption         text NOT NULL CHECK (encryption IN ('nip44', 'nip04')),
    created_at         timestamptz NOT NULL DEFAULT now(),
    revoked_at         timestamptz
);
CREATE UNIQUE INDEX wallet_connections_one_active_idx
    ON wallet_connections (user_id) WHERE revoked_at IS NULL;

CREATE TABLE tips (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- hex(sha256("{github_installation_id}:{comment_id}:{command_index}"))
    idempotency_key    text NOT NULL UNIQUE,
    installation_id    uuid NOT NULL REFERENCES installations (id),
    sender_id          uuid NOT NULL REFERENCES users (id),
    receiver_id        uuid REFERENCES users (id),
    receiver_login     text NOT NULL,
    repo               text NOT NULL,
    thread_number      integer NOT NULL CHECK (thread_number > 0),
    is_pr              boolean NOT NULL,
    comment_id         bigint NOT NULL,
    command_index      smallint NOT NULL CHECK (command_index >= 0),
    kind               text NOT NULL CHECK (kind IN ('now', 'on_merge')),
    amount_sats        bigint NOT NULL CHECK (amount_sats > 0),
    note               text CHECK (char_length(note) <= 140),
    state              text NOT NULL CHECK (state IN (
                           'received', 'awaiting_sender', 'verifying', 'awaiting_confirmation',
                           'held', 'paying', 'pledged', 'paid', 'rejected', 'cancelled',
                           'expired', 'failed', 'fiat_processing', 'fiat_stuck', 'refunded')),
    version            integer NOT NULL DEFAULT 0,
    verified_by        jsonb,
    payment_hash       text UNIQUE,
    preimage           text,
    error_code         text,
    error_detail       text,
    claim_token_hash   text UNIQUE,
    confirm_expires_at timestamptz,
    confirmed_at       timestamptz,
    expires_at         timestamptz,
    email_status       text CHECK (email_status IN (
                           'none', 'pending', 'sent', 'delivered', 'bounced', 'failed')),
    last_notified_at   timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX tips_sender_created_idx ON tips (sender_id, created_at);
CREATE INDEX tips_receiver_state_idx ON tips (receiver_id, state);
CREATE INDEX tips_thread_state_idx ON tips (repo, thread_number, state);
CREATE INDEX tips_state_expires_idx ON tips (state, expires_at);
CREATE TRIGGER tips_set_updated_at BEFORE UPDATE ON tips
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Append-only audit of every state transition, written in the transition's transaction.
CREATE TABLE tip_events (
    id         bigserial PRIMARY KEY,
    tip_id     uuid NOT NULL REFERENCES tips (id),
    from_state text,
    to_state   text NOT NULL,
    detail     jsonb NOT NULL DEFAULT '{}',
    at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX tip_events_tip_at_idx ON tip_events (tip_id, at);

-- The single collapsed MergeTip status comment per thread.
CREATE TABLE bot_comments (
    repo              text NOT NULL,
    thread_number     integer NOT NULL,
    github_comment_id bigint NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (repo, thread_number)
);
CREATE TRIGGER bot_comments_set_updated_at BEFORE UPDATE ON bot_comments
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Postgres job queue, claimed with FOR UPDATE SKIP LOCKED. Completed jobs are deleted; jobs
-- that exhaust their attempts keep a dead_at for inspection.
CREATE TABLE jobs (
    id           bigserial PRIMARY KEY,
    kind         text NOT NULL,
    payload      jsonb NOT NULL DEFAULT '{}',
    run_at       timestamptz NOT NULL DEFAULT now(),
    attempts     integer NOT NULL DEFAULT 0,
    max_attempts integer NOT NULL DEFAULT 3 CHECK (max_attempts > 0),
    locked_at    timestamptz,
    locked_by    text,
    last_error   text,
    dead_at      timestamptz,
    dedupe_key   text,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_ready_idx ON jobs (run_at) WHERE dead_at IS NULL;
CREATE UNIQUE INDEX jobs_dedupe_idx ON jobs (kind, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND dead_at IS NULL;

CREATE TABLE feature_flags (
    key        text PRIMARY KEY,
    enabled    boolean NOT NULL,
    rules      jsonb NOT NULL DEFAULT '{}',
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER feature_flags_set_updated_at BEFORE UPDATE ON feature_flags
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

INSERT INTO feature_flags (key, enabled) VALUES
    ('payments_enabled', true),
    ('fiat_payouts', false);
