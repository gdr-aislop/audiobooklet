//! Per-book series membership and real position ("book 5 of 8"), sourced from
//! `abs_api::Client::get_library_series_summaries` — the only place Audiobookshelf's API exposes
//! a book's series as a clean name and a separate `sequence` (`media.metadata.seriesName`, used
//! elsewhere in this app, is a display string with the number glued on: "Name #5"). The cache
//! this fills (`series_books`) is what the Library groups and orders by, and what Item Detail's
//! "5/8" reads. [`sync_library_series`] runs for every book library as part of the general sync
//! (`sync::sync_all`), best-effort, and Item Detail refreshes it too; [`series_info_for_item`] is
//! a purely local read and never touches the network, which is what makes this offline-friendly
//! after the first fetch: once cached, it's available even without a connection.

use sqlx::SqlitePool;

use abs_storage::repo::series_books::{self, NewSeriesBook};

use crate::error::{CoreError, Result};

/// Fetches a library's full series list and replaces whatever was cached for it
/// (`repo::series_books::replace_all_for_library`). Malformed entries were already filtered out
/// by `get_library_series_summaries` itself.
pub async fn sync_library_series(pool: &SqlitePool, api: &abs_api::Client, server_id: &str, library_id: &str) -> Result<()> {
    let series_list = api.get_library_series_summaries(library_id).await.map_err(|e| match e {
        abs_api::LibraryItemsError::Unauthorized(_) => CoreError::Auth,
        other => CoreError::UnexpectedResponse(other.details()),
    })?;

    let mut rows = Vec::new();
    for series in &series_list {
        for book in &series.books {
            rows.push(NewSeriesBook { series_id: &series.id, series_name: &series.name, item_id: &book.item_id, sequence: book.sequence.as_deref() });
        }
    }

    series_books::replace_all_for_library(pool, server_id, library_id, &rows).await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesInfo {
    pub series_name: String,
    pub sequence: Option<String>,
    pub total_books: i64,
}

/// Thin, local-only pass-through — never touches the network. `None` if the item has no series,
/// or its library's series list hasn't been fetched yet (see [`sync_library_series`]).
pub async fn series_info_for_item(pool: &SqlitePool, server_id: &str, item_id: &str) -> Result<Option<SeriesInfo>> {
    let info = series_books::find_for_item(pool, server_id, item_id).await?;
    Ok(info.map(|i| SeriesInfo { series_name: i.series_name, sequence: i.sequence, total_books: i.total_books }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use abs_storage::db::connect_and_migrate;
    use abs_storage::repo::{items, libraries, servers};
    use chrono::Utc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn pool_with_items(server_uri: &str, item_ids: &[&str]) -> (SqlitePool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = servers::add(&pool, server_uri).await.unwrap();
        libraries::upsert(&pool, libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 })
            .await
            .unwrap();
        for item_id in item_ids {
            items::upsert(
                &pool,
                items::UpsertItem {
                    id: item_id,
                    server_id: &server_id,
                    library_id: "lib-1",
                    title: "Some Book",
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
        }
        (pool, server_id)
    }

    #[tokio::test]
    async fn sync_library_series_stores_every_books_sequence() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/series"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "series-1",
                    "name": "Foundation",
                    "books": [
                        { "id": "item-1", "sequence": "1" },
                        { "id": "item-2", "sequence": "2" }
                    ]
                }]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_items(&mock_server.uri(), &["item-1", "item-2"]).await;
        let api = abs_api::Client::new(&mock_server.uri());
        sync_library_series(&pool, &api, &server_id, "lib-1").await.unwrap();

        let info = series_info_for_item(&pool, &server_id, "item-2").await.unwrap().unwrap();
        assert_eq!(info.series_name, "Foundation");
        assert_eq!(info.sequence, Some("2".to_string()));
        assert_eq!(info.total_books, 2);
    }

    #[tokio::test]
    async fn sync_library_series_running_twice_does_not_duplicate() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/series"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "series-1",
                    "name": "Foundation",
                    "books": [{ "id": "item-1", "sequence": "1" }, { "id": "item-2", "sequence": "2" }]
                }]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_items(&mock_server.uri(), &["item-1", "item-2"]).await;
        let api = abs_api::Client::new(&mock_server.uri());

        sync_library_series(&pool, &api, &server_id, "lib-1").await.unwrap();
        sync_library_series(&pool, &api, &server_id, "lib-1").await.unwrap();

        let info = series_info_for_item(&pool, &server_id, "item-1").await.unwrap().unwrap();
        assert_eq!(info.total_books, 2, "syncing the identical list twice must not duplicate rows");
    }

    #[tokio::test]
    async fn series_info_for_item_is_none_when_never_synced() {
        let mock_server = MockServer::start().await;
        let (pool, server_id) = pool_with_items(&mock_server.uri(), &["item-1"]).await;
        assert!(series_info_for_item(&pool, &server_id, "item-1").await.unwrap().is_none());
    }
}
