-- /remindme: the bot DMs you at a time you name.
--
-- On disk rather than in memory because the whole point is surviving until
-- the reminder is due, and a server restart in between is normal.
CREATE TABLE IF NOT EXISTS reminders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id),
    -- When to send it, ms since epoch.
    due_at INTEGER NOT NULL,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    -- Set once delivered, so a sweep that runs twice can't send twice.
    delivered_at INTEGER
);

-- The sweep asks one question — "what is due and undelivered" — every time it
-- runs, so that is the index.
CREATE INDEX IF NOT EXISTS idx_reminders_due
    ON reminders (delivered_at, due_at);
