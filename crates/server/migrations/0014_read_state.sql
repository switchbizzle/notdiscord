-- Per-user read position in each channel: everything after last_read_id is
-- unread. Powers the sidebar badges and the "NEW" divider.
CREATE TABLE IF NOT EXISTS read_state (
    user_id      INTEGER NOT NULL,
    channel_id   INTEGER NOT NULL,
    last_read_id INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, channel_id)
);

-- Existing members start caught up: nobody wants 500 unread on upgrade day.
INSERT OR IGNORE INTO read_state (user_id, channel_id, last_read_id)
SELECT u.id,
       c.id,
       COALESCE((SELECT MAX(m.id) FROM messages m WHERE m.channel_id = c.id), 0)
FROM users u
CROSS JOIN channels c;
