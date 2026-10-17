-- Listening sessions recorded on this device, for Audiobookshelf's "local session" sync
-- (`POST /api/session/local-all`): what the server's listening history and stats are built from.
-- Progress alone (`PATCH /api/me/progress`) leaves no trace there.
--
-- One row per stretch of listening to one book. A row is "dirty" — it has something the server
-- hasn't confirmed — while `sent_updated_at` differs from `updated_at`; sending it sets the first
-- to the second. Timestamps are this device's clock in milliseconds; the server's clock offset is
-- applied when sending, since it may be unknown when the listening happens (offline). The position
-- columns aren't named `start_time`/`current_time` like the server's fields: `current_time` is an
-- SQLite keyword, and `SELECT current_time` returns the clock.
CREATE TABLE listening_sessions (
    id               TEXT PRIMARY KEY NOT NULL, -- uuid v4, also the server's session id
    account_id       TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    server_id        TEXT NOT NULL,
    item_id          TEXT NOT NULL,
    play_method      INTEGER NOT NULL, -- Audiobookshelf's PlayMethod: 0 streamed, 3 local file
    start_position   REAL NOT NULL,    -- book position the session started at, seconds
    current_position REAL NOT NULL,    -- book position now, seconds
    time_listening   REAL NOT NULL DEFAULT 0, -- seconds actually listened
    duration         REAL NOT NULL DEFAULT 0, -- the book's duration, seconds
    started_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    sent_updated_at  INTEGER,
    closed           INTEGER NOT NULL DEFAULT 0 CHECK (closed IN (0, 1)),
    FOREIGN KEY (server_id, item_id) REFERENCES items(server_id, id) ON DELETE CASCADE
);

CREATE INDEX listening_sessions_dirty_idx ON listening_sessions(account_id, server_id) WHERE sent_updated_at IS NOT updated_at;
CREATE INDEX listening_sessions_item_idx ON listening_sessions(server_id, item_id);
