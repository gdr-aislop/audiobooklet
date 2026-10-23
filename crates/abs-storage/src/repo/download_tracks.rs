//! One row per track download attempt/result — the real unit of network I/O and disk file (see
//! `repo::tracks` for why this replaced the original per-chapter `downloads` table). Tracks
//! `bytes_downloaded` so an interrupted transfer can resume with an HTTP `Range` request instead
//! of restarting, per `docs/design/ui-spec.md`'s "Downloads are resumable" requirement. Backs
//! Item Detail's per-chapter offline markers (computed in `abs-core` from which tracks are
//! `complete`), the Downloads screen's per-item progress/remove rows, and the Home/Library
//! offline-mode toggle's "downloaded fully or partially" filter.

use chrono::Utc;
use sqlx::{FromRow, SqlitePool};

use crate::error::Result;
use crate::models::{DownloadStatus, DownloadTrack};

/// Raw row shape sqlx can decode directly (`status` as `String`) — mapped to the typed
/// `DownloadTrack` (with a real `DownloadStatus`) right after the query, so nothing outside this
/// module ever sees the on-disk string encoding.
#[derive(FromRow)]
struct Row {
    server_id: String,
    item_id: String,
    ino: String,
    file_path: String,
    expected_size_bytes: Option<i64>,
    bytes_downloaded: i64,
    status: String,
    error_reason: Option<String>,
    updated_at: chrono::DateTime<Utc>,
}

impl From<Row> for DownloadTrack {
    fn from(row: Row) -> Self {
        DownloadTrack {
            server_id: row.server_id,
            item_id: row.item_id,
            ino: row.ino,
            file_path: row.file_path,
            expected_size_bytes: row.expected_size_bytes,
            bytes_downloaded: row.bytes_downloaded,
            // A status value that no longer parses (a future format change) is treated as
            // `Failed` rather than panicking or silently pretending it's `Pending` — a corrupted
            // status is closer in spirit to "something went wrong with this row" than "never
            // started", and `Failed` is exactly the state that prompts a retry on next resume.
            status: row.status.parse().unwrap_or(DownloadStatus::Failed),
            error_reason: row.error_reason,
            updated_at: row.updated_at,
        }
    }
}

const SELECT_COLUMNS: &str = "server_id, item_id, ino, file_path, expected_size_bytes, bytes_downloaded, status, error_reason, updated_at";

/// Creates the row if it doesn't exist yet (status `pending`, `bytes_downloaded` 0); a track
/// that's already being tracked is left untouched — this is the "make sure a row exists to resume
/// against" entry point, not an unconditional reset.
pub async fn upsert_pending(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str, file_path: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO download_tracks (server_id, item_id, ino, file_path, bytes_downloaded, status, updated_at)
         VALUES (?, ?, ?, ?, 0, 'pending', ?)
         ON CONFLICT(server_id, item_id, ino) DO NOTHING",
    )
    .bind(server_id)
    .bind(item_id)
    .bind(ino)
    .bind(file_path)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_progress(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str, bytes_downloaded: i64, expected_size_bytes: Option<i64>) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE download_tracks SET bytes_downloaded = ?, expected_size_bytes = COALESCE(?, expected_size_bytes), status = 'downloading', updated_at = ?
         WHERE server_id = ? AND item_id = ? AND ino = ?",
    )
    .bind(bytes_downloaded)
    .bind(expected_size_bytes)
    .bind(now)
    .bind(server_id)
    .bind(item_id)
    .bind(ino)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_complete(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str, final_size_bytes: i64) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE download_tracks SET bytes_downloaded = ?, expected_size_bytes = ?, status = 'complete', error_reason = NULL, updated_at = ?
         WHERE server_id = ? AND item_id = ? AND ino = ?",
    )
    .bind(final_size_bytes)
    .bind(final_size_bytes)
    .bind(now)
    .bind(server_id)
    .bind(item_id)
    .bind(ino)
    .execute(pool)
    .await?;
    Ok(())
}

/// Upsert, not a plain `UPDATE`: a track can fail before any row exists yet — e.g. the very first
/// request for it comes back `404`, before a file path is even known — so this must be able to
/// create the row itself (with an empty `file_path`, meaning "no file was ever written") rather
/// than silently no-op and leave the caller unable to look up what happened.
pub async fn mark_failed(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str, reason: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO download_tracks (server_id, item_id, ino, file_path, bytes_downloaded, status, error_reason, updated_at)
         VALUES (?, ?, ?, '', 0, 'failed', ?, ?)
         ON CONFLICT(server_id, item_id, ino) DO UPDATE SET
            status = 'failed', error_reason = excluded.error_reason, updated_at = excluded.updated_at",
    )
    .bind(server_id)
    .bind(item_id)
    .bind(ino)
    .bind(reason)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// A finished download whose file turned out to be damaged (gone, or not the size recorded when
/// it finished): no longer counted as downloaded anywhere, with `reason` saying why, and its
/// progress reset so downloading it again starts from the first byte rather than "resuming" past
/// the end of a file that can't be trusted. Only a `complete` row is changed, so a download that
/// was meanwhile started again isn't disturbed.
pub async fn mark_damaged(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str, reason: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE download_tracks SET status = 'failed', bytes_downloaded = 0, expected_size_bytes = NULL, error_reason = ?, updated_at = ?
         WHERE server_id = ? AND item_id = ? AND ino = ? AND status = 'complete'",
    )
    .bind(reason)
    .bind(now)
    .bind(server_id)
    .bind(item_id)
    .bind(ino)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str) -> Result<Option<DownloadTrack>> {
    let row: Option<Row> = sqlx::query_as(&format!("SELECT {SELECT_COLUMNS} FROM download_tracks WHERE server_id = ? AND item_id = ? AND ino = ?"))
        .bind(server_id)
        .bind(item_id)
        .bind(ino)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(DownloadTrack::from))
}

pub async fn list_for_item(pool: &SqlitePool, server_id: &str, item_id: &str) -> Result<Vec<DownloadTrack>> {
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {SELECT_COLUMNS} FROM download_tracks WHERE server_id = ? AND item_id = ?"))
        .bind(server_id)
        .bind(item_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(DownloadTrack::from).collect())
}

/// Item ids (scoped to one server) with at least one `complete` track — the query behind the
/// Home/Library offline-mode toggle's filtered view. A `pending`/`downloading`/`failed`-only item
/// (nothing complete yet) deliberately does not count as "downloaded" here.
pub async fn downloaded_item_ids(pool: &SqlitePool, server_id: &str) -> Result<Vec<String>> {
    let ids = sqlx::query_scalar("SELECT DISTINCT item_id FROM download_tracks WHERE server_id = ? AND status = 'complete'")
        .bind(server_id)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

/// "Clear downloaded chapters": remove every download-track row for an item, returning what was
/// removed so the caller can delete the actual files (this repo only tracks what's in the
/// database, same contract the original per-chapter `downloads` table documented).
pub async fn remove_for_item(pool: &SqlitePool, server_id: &str, item_id: &str) -> Result<Vec<DownloadTrack>> {
    let removed = list_for_item(pool, server_id, item_id).await?;
    sqlx::query("DELETE FROM download_tracks WHERE server_id = ? AND item_id = ?")
        .bind(server_id)
        .bind(item_id)
        .execute(pool)
        .await?;
    Ok(removed)
}

/// "Clear all downloads": remove every download-track row for a server (every item), returning
/// what was removed so the caller can delete the actual files — the server-scoped mirror of
/// [`remove_for_item`].
pub async fn remove_for_server(pool: &SqlitePool, server_id: &str) -> Result<Vec<DownloadTrack>> {
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {SELECT_COLUMNS} FROM download_tracks WHERE server_id = ?"))
        .bind(server_id)
        .fetch_all(pool)
        .await?;
    let removed: Vec<DownloadTrack> = rows.into_iter().map(DownloadTrack::from).collect();
    sqlx::query("DELETE FROM download_tracks WHERE server_id = ?").bind(server_id).execute(pool).await?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_and_migrate;
    use crate::repo::{items, libraries, servers, tracks};
    use chrono::Utc;

    async fn pool_with_track(item_id: &str, ino: &str) -> (SqlitePool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = servers::add(&pool, "https://a.example").await.unwrap();
        libraries::upsert(
            &pool,
            libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        items::upsert(
            &pool,
            items::UpsertItem {
                id: item_id,
                server_id: &server_id,
                library_id: "lib-1",
                title: "Project Hail Mary",
                author: None,
                narrator: None,
                description: None,
                duration_seconds: 3600.0,
                added_at: Utc::now(),
                series_name: None,
                genres: &[],
            },
        )
        .await
        .unwrap();
        tracks::upsert_all(&pool, &server_id, item_id, &[tracks::NewTrack { ino, duration_seconds: 3600.0, offset_seconds: 0.0, size_bytes: None }]).await.unwrap();
        (pool, server_id)
    }

    #[tokio::test]
    async fn upsert_pending_then_get_round_trips() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/downloads/ino-1.mp3").await.unwrap();

        let row = get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Pending);
        assert_eq!(row.bytes_downloaded, 0);
        assert_eq!(row.file_path, "/downloads/ino-1.mp3");
    }

    #[tokio::test]
    async fn upsert_pending_does_not_reset_an_in_progress_row() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/downloads/ino-1.mp3").await.unwrap();
        update_progress(&pool, &server_id, "item-1", "ino-1", 5_000, Some(10_000)).await.unwrap();

        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/downloads/ino-1.mp3").await.unwrap();

        let row = get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.bytes_downloaded, 5_000, "re-calling upsert_pending must not discard resumable progress");
        assert_eq!(row.status, DownloadStatus::Downloading);
    }

    #[tokio::test]
    async fn update_progress_sets_downloading_status() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/p.mp3").await.unwrap();
        update_progress(&pool, &server_id, "item-1", "ino-1", 1_234, Some(9_999)).await.unwrap();

        let row = get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Downloading);
        assert_eq!(row.bytes_downloaded, 1_234);
        assert_eq!(row.expected_size_bytes, Some(9_999));
    }

    #[tokio::test]
    async fn mark_complete_clears_any_prior_error() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/p.mp3").await.unwrap();
        mark_failed(&pool, &server_id, "item-1", "ino-1", "connection reset").await.unwrap();

        mark_complete(&pool, &server_id, "item-1", "ino-1", 10_000).await.unwrap();

        let row = get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Complete);
        assert_eq!(row.bytes_downloaded, 10_000);
        assert_eq!(row.expected_size_bytes, Some(10_000));
        assert_eq!(row.error_reason, None);
    }

    #[tokio::test]
    async fn mark_damaged_uncounts_a_complete_download_and_resets_its_progress() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/p.mp3").await.unwrap();
        mark_complete(&pool, &server_id, "item-1", "ino-1", 10_000).await.unwrap();

        mark_damaged(&pool, &server_id, "item-1", "ino-1", "the downloaded file was damaged").await.unwrap();

        let row = get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed);
        assert_eq!(row.bytes_downloaded, 0, "a fresh download must start from the first byte");
        assert_eq!(row.expected_size_bytes, None);
        assert_eq!(row.error_reason.as_deref(), Some("the downloaded file was damaged"));
        assert_eq!(row.file_path, "/p.mp3", "the path is kept, so the next download overwrites the bad file");
    }

    #[tokio::test]
    async fn mark_damaged_leaves_a_download_that_is_running_again_alone() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/p.mp3").await.unwrap();
        update_progress(&pool, &server_id, "item-1", "ino-1", 4_000, Some(10_000)).await.unwrap();

        mark_damaged(&pool, &server_id, "item-1", "ino-1", "damaged").await.unwrap();

        let row = get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Downloading);
        assert_eq!(row.bytes_downloaded, 4_000);
    }

    #[tokio::test]
    async fn mark_failed_creates_a_row_when_none_existed_yet() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;

        mark_failed(&pool, &server_id, "item-1", "ino-1", "HTTP 404 Not Found").await.unwrap();

        let row = get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed);
        assert_eq!(row.error_reason.as_deref(), Some("HTTP 404 Not Found"));
        assert_eq!(row.bytes_downloaded, 0);
    }

    #[tokio::test]
    async fn mark_failed_records_the_reason_without_losing_progress() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/p.mp3").await.unwrap();
        update_progress(&pool, &server_id, "item-1", "ino-1", 4_096, None).await.unwrap();

        mark_failed(&pool, &server_id, "item-1", "ino-1", "connection reset").await.unwrap();

        let row = get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed);
        assert_eq!(row.error_reason.as_deref(), Some("connection reset"));
        assert_eq!(row.bytes_downloaded, 4_096, "a failure must not discard bytes already on disk");
    }

    #[tokio::test]
    async fn downloaded_item_ids_includes_only_complete_tracks() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/p.mp3").await.unwrap();
        update_progress(&pool, &server_id, "item-1", "ino-1", 1, None).await.unwrap();

        assert!(downloaded_item_ids(&pool, &server_id).await.unwrap().is_empty(), "downloading-but-not-complete should not count yet");

        mark_complete(&pool, &server_id, "item-1", "ino-1", 10).await.unwrap();
        assert_eq!(downloaded_item_ids(&pool, &server_id).await.unwrap(), vec!["item-1"]);
    }

    #[tokio::test]
    async fn remove_for_item_returns_removed_rows_for_the_caller_to_delete_files() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/downloads/ino-1.mp3").await.unwrap();
        mark_complete(&pool, &server_id, "item-1", "ino-1", 10).await.unwrap();

        let removed = remove_for_item(&pool, &server_id, "item-1").await.unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].file_path, "/downloads/ino-1.mp3");
        assert!(list_for_item(&pool, &server_id, "item-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn removing_a_track_row_cascades_to_its_download() {
        let (pool, server_id) = pool_with_track("item-1", "ino-1").await;
        upsert_pending(&pool, &server_id, "item-1", "ino-1", "/p.mp3").await.unwrap();

        // Re-syncing tracks (`tracks::upsert_all`) deletes and re-inserts every row for the item,
        // so a download referencing a now-gone track must cascade-delete rather than orphan.
        tracks::upsert_all(&pool, &server_id, "item-1", &[]).await.unwrap();

        assert!(list_for_item(&pool, &server_id, "item-1").await.unwrap().is_empty());
    }
}
