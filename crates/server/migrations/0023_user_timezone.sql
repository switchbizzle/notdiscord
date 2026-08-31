-- Each person's UTC offset, so "/remindme 9/7/2026 dentist" means 9am where
-- THEY are rather than 9am UTC.
--
-- Minutes to ADD to UTC to get their local time, so US Eastern in summer is
-- -240. Zero is a correct default: it means UTC, which is exactly what the
-- server assumed before anyone told it otherwise.
ALTER TABLE users ADD COLUMN tz_offset_minutes INTEGER NOT NULL DEFAULT 0;
