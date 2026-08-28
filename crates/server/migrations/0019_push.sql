-- Web Push subscriptions (one per browser/device) and how much each person
-- wants to be told about.
CREATE TABLE IF NOT EXISTS push_subscriptions (
    endpoint   TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL,
    p256dh     TEXT NOT NULL,
    auth       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS push_subscriptions_user ON push_subscriptions(user_id);

-- 'all' | 'mentions' | 'none'. Mentions (and DMs) is the sane default.
ALTER TABLE users ADD COLUMN notify_level TEXT NOT NULL DEFAULT 'mentions';
