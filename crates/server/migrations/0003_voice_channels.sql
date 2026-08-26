ALTER TABLE channels ADD COLUMN kind TEXT NOT NULL DEFAULT 'text';

INSERT INTO channels (name, kind, created_at) VALUES ('lounge', 'voice', strftime('%s', 'now') * 1000);
