-- Free-text custom status ("away", "playing X"), shown under the username.
ALTER TABLE users ADD COLUMN status_text TEXT;
