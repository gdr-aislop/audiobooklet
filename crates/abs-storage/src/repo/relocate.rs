//! Rewrites the absolute file paths stored in the database when the directories they point into
//! move — once, when the app's directories moved with its rename (see
//! [`crate::paths::AppPaths::migrate_legacy_dirs`]).

use std::path::Path;

use sqlx::SqlitePool;

use crate::error::Result;

/// Every column that holds an absolute path into the app's own directories (a client certificate
/// usually lives elsewhere, and is then left alone like any other non-matching row).
const PATH_COLUMNS: [(&str, &str); 3] = [
    ("items", "cover_cache_path"),
    ("download_tracks", "file_path"),
    ("servers", "client_cert_path"),
];

/// Replaces the leading `from/` of every stored path with `to/`, in one transaction, and returns
/// how many rows changed. Paths outside `from` (and a sibling such as `from-old/`) are untouched,
/// so running it again changes nothing. Matching compares the prefix as plain text rather than with
/// `LIKE`, whose `_` and `%` wildcards occur in real paths (`gdr_aislop`).
pub async fn rebase_paths(pool: &SqlitePool, from: &Path, to: &Path) -> Result<u64> {
    let from = format!("{}/", from.to_string_lossy().trim_end_matches('/'));
    let to = format!("{}/", to.to_string_lossy().trim_end_matches('/'));
    let mut tx = pool.begin().await?;
    let mut changed = 0;
    for (table, column) in PATH_COLUMNS {
        let sql = format!(
            "UPDATE {table} SET {column} = ?1 || substr({column}, length(?2) + 1) WHERE substr({column}, 1, length(?2)) = ?2"
        );
        changed += sqlx::query(&sql).bind(&to).bind(&from).execute(&mut *tx).await?.rows_affected();
    }
    tx.commit().await?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_and_migrate;

    const OLD: &str = "/home/jan/.local/share/io.github.gdr_aislop.abs-app";
    const NEW: &str = "/home/jan/.local/share/io.github.gdr_aislop.audiobooklet";

    async fn seeded_pool() -> SqlitePool {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        // Only the path columns matter here, so skip building the libraries/tracks the rows'
        // foreign keys would otherwise require.
        let mut conn = pool.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await.unwrap();
        let rows = [
            format!("INSERT INTO servers (id, url, created_at, client_cert_path) VALUES ('s1', 'https://a', 'now', '{OLD}/certs/me.p12')"),
            "INSERT INTO servers (id, url, created_at, client_cert_path) VALUES ('s2', 'https://b', 'now', '/home/jan/me.p12')".to_string(),
            format!("INSERT INTO items (id, server_id, library_id, title, cover_cache_path, added_at, synced_at) VALUES ('i1', 's1', 'l', 'T', '{OLD}/covers/s1/i1.jpg', 'now', 'now')"),
            "INSERT INTO items (id, server_id, library_id, title, cover_cache_path, added_at, synced_at) VALUES ('i2', 's1', 'l', 'T', NULL, 'now', 'now')".to_string(),
            format!("INSERT INTO items (id, server_id, library_id, title, cover_cache_path, added_at, synced_at) VALUES ('i3', 's1', 'l', 'T', '{OLD}-old/covers/i3.jpg', 'now', 'now')"),
            format!("INSERT INTO download_tracks (server_id, item_id, ino, file_path, updated_at) VALUES ('s1', 'i1', '7', '{OLD}/downloads/s1/i1/7.mp3', 'now')"),
            "INSERT INTO download_tracks (server_id, item_id, ino, file_path, updated_at) VALUES ('s1', 'i1', '8', '/home/jan/.local/share/ioXgithubXgdrXaislopXabs-app/8.mp3', 'now')".to_string(),
        ];
        for row in rows {
            sqlx::query(&row).execute(&mut *conn).await.unwrap();
        }
        drop(conn);
        pool
    }

    async fn text(pool: &SqlitePool, sql: &str) -> Option<String> {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn rewrites_only_paths_inside_the_old_directory() {
        let pool = seeded_pool().await;
        let changed = rebase_paths(&pool, Path::new(OLD), Path::new(NEW)).await.unwrap();
        assert_eq!(changed, 3, "the cover, the download and the certificate under the old directory");

        assert_eq!(text(&pool, "SELECT cover_cache_path FROM items WHERE id = 'i1'").await.unwrap(), format!("{NEW}/covers/s1/i1.jpg"));
        assert_eq!(text(&pool, "SELECT file_path FROM download_tracks WHERE ino = '7'").await.unwrap(), format!("{NEW}/downloads/s1/i1/7.mp3"));
        assert_eq!(text(&pool, "SELECT client_cert_path FROM servers WHERE id = 's1'").await.unwrap(), format!("{NEW}/certs/me.p12"));

        assert_eq!(text(&pool, "SELECT client_cert_path FROM servers WHERE id = 's2'").await.unwrap(), "/home/jan/me.p12", "outside: untouched");
        assert_eq!(text(&pool, "SELECT cover_cache_path FROM items WHERE id = 'i2'").await, None, "NULL stays NULL");
        assert_eq!(
            text(&pool, "SELECT cover_cache_path FROM items WHERE id = 'i3'").await.unwrap(),
            format!("{OLD}-old/covers/i3.jpg"),
            "a sibling directory whose name merely starts the same is not inside it"
        );
        assert_eq!(
            text(&pool, "SELECT file_path FROM download_tracks WHERE ino = '8'").await.unwrap(),
            "/home/jan/.local/share/ioXgithubXgdrXaislopXabs-app/8.mp3",
            "`.` and `_` are compared literally, never as wildcards"
        );
    }

    #[tokio::test]
    async fn running_it_again_changes_nothing() {
        let pool = seeded_pool().await;
        rebase_paths(&pool, Path::new(OLD), Path::new(NEW)).await.unwrap();
        assert_eq!(rebase_paths(&pool, Path::new(OLD), Path::new(NEW)).await.unwrap(), 0);
        assert_eq!(
            rebase_paths(&pool, Path::new(&format!("{OLD}/")), Path::new(NEW)).await.unwrap(),
            0,
            "a trailing slash on the input is the same directory"
        );
    }
}
