use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

use crate::error::Result;

/// Open (creating if necessary) the SQLite database at `path` and run any pending migrations.
/// This is the one place a connection pool gets constructed — callers never build their own.
pub async fn connect_and_migrate(path: &Path) -> Result<SqlitePool> {
    connect_and_migrate_with(path, DbProfile::Default).await
}

/// How much memory the database may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbProfile {
    Default,
    /// "Low memory mode": fewer connections and a small page cache each (SQLite's default is
    /// about 2 MB per connection).
    LowMemory,
}

impl DbProfile {
    fn max_connections(self) -> u32 {
        match self {
            DbProfile::Default => MAX_CONNECTIONS,
            DbProfile::LowMemory => 3,
        }
    }
}

/// [`connect_and_migrate`] with an explicit memory [`DbProfile`].
pub async fn connect_and_migrate_with(path: &Path, profile: DbProfile) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        // WAL: readers never block on (or behind) the writer. Without this, the pool's
        // concurrent readers queue behind any write transaction with the busy handler, which
        // showed up in the wild as multi-second point SELECTs/INSERTs and a saturated pool
        // (every connection stuck waiting on the file lock) freezing the UI's async work.
        .journal_mode(SqliteJournalMode::Wal)
        // WAL's standard companion: fsync only at checkpoints, not per commit — durability
        // vs. speed tradeoff is a non-issue for cache-and-progress data.
        .synchronous(SqliteSynchronous::Normal)
        // Explicit rather than the default: a contended write waits (up to this long) instead
        // of erroring with "database is locked". 15s (up from an original 5s) gives a
        // UI-triggered write enough room to ride out the auto-sync cycle's writes without
        // erroring — a user action landing mid-sync was observed timing out under the old
        // value even though the writer released the lock well within 15s.
        .busy_timeout(Duration::from_secs(15));
    // Negative: kibibytes. 256 KiB per connection instead of the default of about 2 MiB.
    let options = if profile == DbProfile::LowMemory { options.pragma("cache_size", "-256") } else { options };

    let pool = SqlitePoolOptions::new()
        .max_connections(profile.max_connections())
        // Explicit rather than sqlx's undocumented-at-this-call-site 30s default: kept just
        // under it so a pool-exhaustion failure surfaces a little before that default would,
        // while still comfortably outlasting normal sync-vs-UI-action contention.
        .acquire_timeout(Duration::from_secs(20))
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    spawn_saturation_probe(pool.clone(), profile.max_connections());

    Ok(pool)
}

const MAX_CONNECTIONS: u32 = 5;
const SATURATION_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Whether every connection the pool may have is open and checked out — anything asking for one
/// now waits. (A slow `acquire` with this *not* holding points elsewhere, e.g. at a blocked
/// caller; see `app/src/perf.rs`.)
fn is_saturated(size: u32, idle: usize, max: u32) -> bool {
    size >= max && idle == 0
}

/// Samples the pool once a second from a Tokio worker — independent of the GTK main loop that
/// polls most queries — and warns when it has been saturated for two samples in a row, once per
/// episode. Stops when the pool is closed.
fn spawn_saturation_probe(pool: SqlitePool, max_connections: u32) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    runtime.spawn(async move {
        let mut saturated_samples = 0_u32;
        loop {
            tokio::time::sleep(SATURATION_PROBE_INTERVAL).await;
            if pool.is_closed() {
                return;
            }
            if is_saturated(pool.size(), pool.num_idle(), max_connections) {
                saturated_samples += 1;
                if saturated_samples == 2 {
                    tracing::warn!(connections = pool.size(), "the database pool is saturated: every connection has been busy for over a second");
                }
            } else {
                saturated_samples = 0;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturated_means_every_connection_open_and_none_idle() {
        assert!(is_saturated(5, 0, 5));
        assert!(!is_saturated(5, 1, 5), "one idle connection is not saturation");
        assert!(!is_saturated(3, 0, 5), "the pool can still open more");
        assert!(!is_saturated(0, 0, 5));
    }

    #[tokio::test]
    async fn the_low_memory_profile_has_fewer_connections_and_a_small_page_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate_with(&tmp.path().join("db.sqlite3"), DbProfile::LowMemory).await.unwrap();
        assert_eq!(pool.options().get_max_connections(), 3);
        let cache_size: i64 = sqlx::query_scalar("PRAGMA cache_size").fetch_one(&pool).await.unwrap();
        assert_eq!(cache_size, -256);

        let default_pool = connect_and_migrate(&tmp.path().join("other.sqlite3")).await.unwrap();
        assert_eq!(default_pool.options().get_max_connections(), 5);
        let default_cache: i64 = sqlx::query_scalar("PRAGMA cache_size").fetch_one(&default_pool).await.unwrap();
        assert_ne!(default_cache, -256);
    }

    #[tokio::test]
    async fn connect_and_migrate_creates_the_database_file() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("nested").join("db.sqlite3");

        assert!(!db_path.exists());
        let _pool = connect_and_migrate(&db_path).await.expect("connect+migrate");
        assert!(db_path.exists(), "the db file should be created on disk");
    }

    #[tokio::test]
    async fn connect_and_migrate_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("db.sqlite3");

        let _pool1 = connect_and_migrate(&db_path).await.expect("first connect");
        // Re-opening and re-migrating an already-migrated database must not error.
        let _pool2 = connect_and_migrate(&db_path)
            .await
            .expect("second connect against the same, already-migrated db");
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3"))
            .await
            .unwrap();

        // Inserting a library referencing a server that doesn't exist must fail, proving
        // `foreign_keys(true)` actually took effect (SQLite ignores FK constraints by default).
        let result = sqlx::query(
            "INSERT INTO libraries (id, server_id, name, media_type, display_order, synced_at)
             VALUES ('lib', 'no-such-server', 'Lib', 'book', 1, '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await;

        assert!(result.is_err(), "FK violation should be rejected");
    }

    #[tokio::test]
    async fn expected_tables_exist_after_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3"))
            .await
            .unwrap();

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_sqlx_%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        for expected in [
            "servers", "accounts", "libraries", "items", "chapters", "progress", "tracks",
            "download_tracks", "settings", "bookmarks",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "expected table `{expected}` to exist, found: {tables:?}"
            );
        }
    }

    /// Query plans for the queries the app runs often, and for the lookups SQLite makes on a
    /// parent's delete to find child rows (foreign-key cascades). A bare `SCAN` of a table in
    /// any of them means a missing index; the scans left alone (tiny tables, one-off or rare
    /// queries) are not listed.
    #[tokio::test]
    async fn hot_queries_and_foreign_key_lookups_use_an_index() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();

        let queries = [
            "SELECT * FROM items WHERE server_id = ? AND library_id = ? AND has_audio = 1 ORDER BY added_at DESC",
            "SELECT id, cover_cache_path FROM items WHERE server_id = ? AND library_id = ?",
            "SELECT * FROM items WHERE server_id = ? AND id = ?",
            "SELECT * FROM libraries WHERE server_id = ? ORDER BY display_order ASC",
            "SELECT * FROM accounts WHERE is_active = 1",
            "SELECT * FROM accounts WHERE server_id = ? ORDER BY created_at ASC",
            "SELECT * FROM progress WHERE account_id = ? AND server_id = ? AND item_id = ?",
            "SELECT * FROM progress WHERE account_id = ? AND needs_push = 1",
            "SELECT * FROM progress WHERE account_id = ? ORDER BY updated_at DESC LIMIT ?",
            "SELECT * FROM chapters WHERE server_id = ? AND item_id = ? ORDER BY chapter_index ASC",
            "SELECT * FROM chapters WHERE server_id = ? AND item_id = ? AND start_seconds <= ? AND ? < end_seconds LIMIT 1",
            "SELECT * FROM tracks WHERE server_id = ? AND item_id = ? ORDER BY track_index ASC",
            "SELECT * FROM series_books WHERE server_id = ? AND item_id = ? LIMIT 1",
            "SELECT * FROM series_books WHERE server_id = ? AND series_id = ?",
            "DELETE FROM series_books WHERE server_id = ? AND library_id = ?",
            "SELECT * FROM download_tracks WHERE server_id = ? AND item_id = ? AND ino = ?",
            "SELECT * FROM download_tracks WHERE server_id = ? AND item_id = ?",
            "SELECT DISTINCT item_id FROM download_tracks WHERE server_id = ? AND status = 'complete'",
            // What deleting a parent row looks up in each child table.
            "SELECT 1 FROM progress WHERE server_id = ? AND item_id = ?",
            "SELECT 1 FROM bookmarks WHERE server_id = ? AND item_id = ?",
            "SELECT 1 FROM chapters WHERE server_id = ? AND item_id = ?",
            "SELECT 1 FROM tracks WHERE server_id = ? AND item_id = ?",
            "SELECT 1 FROM series_books WHERE server_id = ? AND item_id = ?",
            "SELECT 1 FROM progress WHERE account_id = ?",
            "SELECT 1 FROM bookmarks WHERE account_id = ?",
            "SELECT 1 FROM accounts WHERE server_id = ?",
            "SELECT 1 FROM libraries WHERE server_id = ?",
            "SELECT 1 FROM items WHERE server_id = ? AND library_id = ?",
            "SELECT 1 FROM download_tracks WHERE server_id = ? AND item_id = ? AND ino = ?",
        ];
        for query in queries {
            let plan: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(&format!("EXPLAIN QUERY PLAN {query}")).fetch_all(&pool).await.unwrap();
            for (_, _, _, detail) in &plan {
                assert!(!detail.starts_with("SCAN "), "`{query}` scans: {plan:?}");
            }
        }
    }
}
