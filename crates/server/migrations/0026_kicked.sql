-- Kicked out of the server, as distinct from banned.
--
-- A ban is a door held shut: the login is refused and only an admin can
-- open it again. A kick puts someone back outside the invite: they can
-- return, but only with the code, so changing the code makes it permanent
-- without anyone having to decide that up front.
--
-- Clears itself when they rejoin, which is why it needs no admin undo.
ALTER TABLE users ADD COLUMN kicked INTEGER NOT NULL DEFAULT 0;
