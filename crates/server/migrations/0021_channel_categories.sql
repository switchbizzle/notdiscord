-- Collapsible groups in the channel list. A channel belongs to at most one,
-- and channels with none sit at the top of the sidebar the way they always
-- have — so an existing server looks unchanged until somebody makes a
-- category.
CREATE TABLE IF NOT EXISTS channel_categories (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL,
    -- Sidebar order. Ties break by id, so a fresh category lands at the end.
    position   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

ALTER TABLE channels ADD COLUMN category_id INTEGER REFERENCES channel_categories(id);
