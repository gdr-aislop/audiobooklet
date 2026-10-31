# Why the library sync is not a "delta" sync

A Library load downloads every book's record from the server and rewrites the matching rows. For a
library of a few hundred books that is a few seconds on a phone, so skipping unchanged work looks
attractive. We looked at it closely and decided against it. This records why, so nobody has to
re-derive it.

## What a skip could save

- **Item-level skip** (don't write a book whose record is unchanged): the list still has to be
  downloaded and parsed to know that, so it saves *only local database writes*: about two writes per
  book. No network time, no parsing time.
- **Library-level skip** (don't request a library's list at all when the server says nothing
  changed): the only variant that saves network time.

## Why not the server's timestamps

Checked against the Audiobookshelf server source (master `edb1f7e`, Oct 2026):

- A library's `lastUpdate` in `GET /api/libraries` is just the library row's own `updatedAt`
  (`server/models/Library.js`, `toOldJSON`). The library row is saved only by a finished full scan
  (`server/scanner/LibraryScanner.js`), by editing or reordering the library
  (`server/controllers/LibraryController.js`), and by removing a custom metadata provider. It is
  **not** saved when books are added by the folder watcher, edited, deleted, moved between
  libraries, or when a user's access changes. A library-level skip keyed on it would hide new books.
- An item's `updatedAt` is not touched when an author is renamed or merged
  (`server/controllers/AuthorController.js` rewrites `media.authors`, the BookAuthor join table and the
  metadata file, but never saves the library item). The list's `authorName` changes while `updatedAt`
  doesn't, so an `updatedAt`-based skip would keep the old name on the phone indefinitely. Series and
  bulk genre/tag renames probably behave the same; not checked.

## What we do instead

Make the writes cheaper without ever deciding to skip one (so there is no staleness to get wrong):

- `abs_core::sync` writes each library's books in transactions of 100 (`BATCH_SIZE`) instead of two
  autocommit writes per book. The list is fetched and parsed before any transaction begins, so the
  write lock is never held while waiting for the network; each chunk is all-or-nothing, and every
  write is an idempotent upsert, so an interrupted sync is finished by the next one.
- Each sync logs one line, e.g.
  `library sync timings libraries=1 items=249 fetch_ms=… write_ms=… longest_chunk_ms=… tidy_ms=… batched=true`,
  to see where the time goes on a real device.
- `ABS_SYNC_BATCH=0` in the environment turns batching off (one autocommit per statement, as
  before) to compare the two on the same build.

If, after batching, `write_ms` is still a large share of a sync, a content fingerprint per book
(skip the writes when the stored hash equals the hash of what we would write) is the only safe
item-level skip: it doesn't depend on the server's timestamps. It would need to be invalidated by
app updates, written in the same transaction as the data, and cleared by every other writer of
those columns. It saves writes only, so it is worth it only if the measurements say writes matter.
