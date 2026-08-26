CREATE TABLE tags (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    color      TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE user_tags (
    user_id INTEGER NOT NULL REFERENCES users(id),
    tag_id  INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    UNIQUE(user_id, tag_id)
);
