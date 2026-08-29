-- Channels a person doesn't want to hear from. A mute silences the noise —
-- push, the notification sound, the ping toast — without hiding the channel
-- or its messages, and it's per person, not per server.
CREATE TABLE IF NOT EXISTS channel_mutes (
    user_id    INTEGER NOT NULL REFERENCES users(id),
    channel_id INTEGER NOT NULL REFERENCES channels(id),
    muted_at   INTEGER NOT NULL,
    PRIMARY KEY (user_id, channel_id)
);
