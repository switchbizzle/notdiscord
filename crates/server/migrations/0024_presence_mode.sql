-- How a person chooses to appear, as opposed to whether they happen to hold
-- a socket. "online" | "idle" | "dnd" | "invisible", defaulting to online so
-- every existing account keeps behaving exactly as it does today.
--
-- Deliberately separate from status_text (0018): that is a sentence about
-- what you are doing, this is a switch that changes how the server treats
-- you — dnd suppresses push, invisible reports you offline to everybody else.
ALTER TABLE users ADD COLUMN presence_mode TEXT NOT NULL DEFAULT 'online';
