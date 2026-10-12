-- A library the server no longer lists, kept only because books in it are downloaded (they stay
-- playable offline until the user deletes them). Sync never asks the server for its items, and
-- Home doesn't list it; a library that comes back on the server clears the flag on upsert.
ALTER TABLE libraries ADD COLUMN removed_on_server INTEGER NOT NULL DEFAULT 0 CHECK (removed_on_server IN (0, 1));

-- Whether the item has audio this app can play. Podcasts (episodes, no audio files of their own)
-- and e-book-only items are 0, and are left out of every list.
ALTER TABLE items ADD COLUMN has_audio INTEGER NOT NULL DEFAULT 1 CHECK (has_audio IN (0, 1));

-- When the account was last made the active one: which account to fall back to when the active
-- one is signed out or its server removed. NULL for accounts never switched to since this column
-- was added (they sort after every stamped one, by creation time).
ALTER TABLE accounts ADD COLUMN last_active_at TEXT;
