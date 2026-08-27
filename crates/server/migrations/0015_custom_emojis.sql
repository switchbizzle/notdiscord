-- Server emojis: uploaded once, usable inline as :name: anywhere in chat.
CREATE TABLE IF NOT EXISTS custom_emojis (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE,
    url        TEXT NOT NULL,
    creator_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
