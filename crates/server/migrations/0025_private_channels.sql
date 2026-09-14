-- Private channels. Everyone still sees one in the list, locked, the way
-- Discord shows a channel you can't open; only its members and the server's
-- owner can read it, post in it, or join its voice room.
ALTER TABLE channels ADD COLUMN private INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS channel_members (
    channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    UNIQUE(channel_id, user_id)
);
