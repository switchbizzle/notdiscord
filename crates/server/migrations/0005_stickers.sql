CREATE TABLE stickers (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL,
    url        TEXT NOT NULL,
    creator_id INTEGER NOT NULL REFERENCES users(id),
    created_at INTEGER NOT NULL
);
