CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE sessions (
    token      TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    created_at INTEGER NOT NULL
);

CREATE TABLE channels (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    created_at INTEGER NOT NULL
);

CREATE TABLE messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id INTEGER NOT NULL REFERENCES channels(id),
    author_id  INTEGER NOT NULL REFERENCES users(id),
    content    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_messages_channel ON messages(channel_id, id);

INSERT INTO channels (name, created_at) VALUES ('general', strftime('%s', 'now') * 1000);
