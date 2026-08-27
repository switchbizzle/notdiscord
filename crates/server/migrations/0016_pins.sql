-- Pinned messages: a message is pinned iff pinned_at is set. pinned_by is
-- kept for the pin list ("pinned by X"), not for permissions.
ALTER TABLE messages ADD COLUMN pinned_at INTEGER;
ALTER TABLE messages ADD COLUMN pinned_by INTEGER;
