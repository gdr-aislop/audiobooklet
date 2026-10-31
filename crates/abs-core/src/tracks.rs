//! Persists what `streaming::resolve_stream_target` already fetches, so track metadata (which
//! `ino`s an item has, their durations/offsets) is available offline — for download planning and,
//! later, offline-availability checks — without a second network round trip.

use sqlx::SqlitePool;

use crate::error::Result;
use crate::streaming::StreamTrack;

/// A plain, storage-shape-free view of a cached track — callers outside this module never need to
/// know it's backed by `abs_storage::models::Track`.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackRef {
    pub ino: String,
    pub track_index: i64,
    pub duration_seconds: f64,
    pub offset_seconds: f64,
    /// The server-reported file size, when known — what download size estimates are computed
    /// from. `None` is "unknown", never an error.
    pub size_bytes: Option<u64>,
}

pub async fn sync_item_tracks(pool: &SqlitePool, server_id: &str, item_id: &str, tracks: &[StreamTrack]) -> Result<()> {
    let new_tracks: Vec<abs_storage::repo::tracks::NewTrack> = tracks
        .iter()
        .map(|t| abs_storage::repo::tracks::NewTrack { ino: &t.ino, duration_seconds: t.duration_seconds, offset_seconds: t.offset_seconds, size_bytes: t.size_bytes })
        .collect();
    abs_storage::repo::tracks::upsert_all(pool, server_id, item_id, &new_tracks).await?;
    Ok(())
}

/// Like [`sync_item_tracks`], but only adds what's missing and fills in unknown sizes; see
/// `abs_storage::repo::tracks::cache_missing` for why a caller that merely saw the item's files must
/// use this one (it can never delete a download's records, so it's safe even while a download of the
/// item starts).
pub async fn cache_missing_tracks(pool: &SqlitePool, server_id: &str, item_id: &str, tracks: &[StreamTrack]) -> Result<()> {
    let new_tracks: Vec<abs_storage::repo::tracks::NewTrack> = tracks
        .iter()
        .map(|t| abs_storage::repo::tracks::NewTrack { ino: &t.ino, duration_seconds: t.duration_seconds, offset_seconds: t.offset_seconds, size_bytes: t.size_bytes })
        .collect();
    abs_storage::repo::tracks::cache_missing(pool, server_id, item_id, &new_tracks).await?;
    Ok(())
}

pub async fn cached_tracks(pool: &SqlitePool, server_id: &str, item_id: &str) -> Result<Vec<TrackRef>> {
    let tracks = abs_storage::repo::tracks::list_for_item(pool, server_id, item_id).await?;
    Ok(tracks
        .into_iter()
        .map(|t| TrackRef {
            ino: t.ino,
            track_index: t.track_index,
            duration_seconds: t.duration_seconds,
            offset_seconds: t.offset_seconds,
            size_bytes: t.size_bytes.and_then(|s| u64::try_from(s).ok()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use abs_storage::repo::{items, libraries, servers};

    async fn pool_with_item() -> (SqlitePool, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
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
                id: "item-1",
                server_id: &server_id,
                library_id: "lib-1",
                title: "Project Hail Mary",
                author: None,
                narrator: None,
                description: None,
                duration_seconds: 3600.0,
                added_at: chrono::Utc::now(),
                series_name: None,
                genres: &[],
            },
        )
        .await
        .unwrap();
        (pool, server_id, "item-1".to_string())
    }

    #[tokio::test]
    async fn sync_item_tracks_then_cached_tracks_round_trips() {
        let (pool, server_id, item_id) = pool_with_item().await;
        let tracks = vec![
            StreamTrack { ino: "111".into(), url: "u1".into(), duration_seconds: 1800.0, offset_seconds: 0.0, size_bytes: Some(1) },
            StreamTrack { ino: "222".into(), url: "u2".into(), duration_seconds: 1800.0, offset_seconds: 1800.0, size_bytes: Some(2) },
        ];

        sync_item_tracks(&pool, &server_id, &item_id, &tracks).await.unwrap();
        let cached = cached_tracks(&pool, &server_id, &item_id).await.unwrap();

        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].ino, "111");
        assert_eq!(cached[1].offset_seconds, 1800.0);
    }

    #[tokio::test]
    async fn syncing_again_replaces_the_previous_track_list() {
        let (pool, server_id, item_id) = pool_with_item().await;
        let first = vec![StreamTrack { ino: "111".into(), url: "u1".into(), duration_seconds: 3600.0, offset_seconds: 0.0, size_bytes: None }];
        let second = vec![
            StreamTrack { ino: "222".into(), url: "u2".into(), duration_seconds: 1800.0, offset_seconds: 0.0, size_bytes: None },
            StreamTrack { ino: "333".into(), url: "u3".into(), duration_seconds: 1800.0, offset_seconds: 1800.0, size_bytes: None },
        ];

        sync_item_tracks(&pool, &server_id, &item_id, &first).await.unwrap();
        sync_item_tracks(&pool, &server_id, &item_id, &second).await.unwrap();

        let cached = cached_tracks(&pool, &server_id, &item_id).await.unwrap();
        assert_eq!(cached.len(), 2);
        assert!(cached.iter().all(|t| t.ino != "111"));
    }
}
