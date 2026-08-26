-- NotBot's long-term memory, one row per channel: distilled notes about
-- everything before compacted_to; messages after that id are fed raw.
CREATE TABLE IF NOT EXISTS bot_memory (
    channel_id   INTEGER PRIMARY KEY,
    notes        TEXT NOT NULL DEFAULT '',
    compacted_to INTEGER NOT NULL DEFAULT 0
);
