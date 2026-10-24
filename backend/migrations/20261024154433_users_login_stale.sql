-- GitHub logins can be renamed and then claimed by another account. A row's login is only what
-- we last saw for that account; once another account is seen with the same login, the old row's
-- copy is stale and must never be used to route a tip. At most one live row holds each login.

ALTER TABLE users ADD COLUMN login_stale boolean NOT NULL DEFAULT false;

CREATE UNIQUE INDEX users_current_login_idx ON users (lower(github_login))
    WHERE NOT login_stale AND deleted_at IS NULL;
