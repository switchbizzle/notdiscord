ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'member';
ALTER TABLE users ADD COLUMN banned INTEGER NOT NULL DEFAULT 0;

-- The first account on the server is the owner and starts as admin.
UPDATE users SET role = 'admin' WHERE id = (SELECT MIN(id) FROM users);
