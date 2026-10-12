-- Indexes for foreign-key child lookups. With `foreign_keys(true)`, deleting a parent row makes
-- SQLite look up its children, and these two lookups had no index to use:
--
-- * `progress` is keyed (account_id, server_id, item_id), but its foreign key to `items` is
--   (server_id, item_id): deleting a book scanned the whole table. Sync deletes every book the
--   server stopped listing, one at a time.
-- * `bookmarks` had an index for its `items` foreign key only: deleting an account (sign-out,
--   removing a server) scanned the whole table. The extra columns also serve "this account's
--   bookmarks in this book".
CREATE INDEX progress_item_idx ON progress(server_id, item_id);
CREATE INDEX bookmarks_account_idx ON bookmarks(account_id, server_id, item_id);
