CREATE TABLE dm_members (
    channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    UNIQUE(channel_id, user_id)
);

CREATE INDEX idx_dm_members_user ON dm_members(user_id);
