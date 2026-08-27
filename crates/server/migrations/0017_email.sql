-- Verified email per user (private — never exposed through shared::User),
-- and one-time codes for verifying an address or resetting a password.
ALTER TABLE users ADD COLUMN email TEXT;
ALTER TABLE users ADD COLUMN email_verified INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS mail_codes (
    user_id    INTEGER NOT NULL,
    email      TEXT NOT NULL,
    code       TEXT NOT NULL,
    purpose    TEXT NOT NULL,             -- 'verify' | 'reset'
    expires_at INTEGER NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, purpose)
);
