-- Cached OpenGraph cards for links posted in chat. Rows expire (by fetched_at)
-- well before the thumbnail they point at can be swept by upload retention.
CREATE TABLE IF NOT EXISTS link_previews (
    url          TEXT PRIMARY KEY,
    -- 0 = the link has no usable preview; cached so we stop refetching it.
    ok           INTEGER NOT NULL DEFAULT 1,
    title        TEXT NOT NULL DEFAULT '',
    description  TEXT NOT NULL DEFAULT '',
    image        TEXT,
    site_name    TEXT NOT NULL DEFAULT '',
    embed        TEXT,
    embed_height INTEGER NOT NULL DEFAULT 0,
    fetched_at   INTEGER NOT NULL
);
