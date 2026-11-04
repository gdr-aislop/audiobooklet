//! Per-book series membership and real position ("book 5 of 8"), sourced from
//! `abs_api::Client::get_library_series_summaries` — the only place Audiobookshelf's API exposes
//! a book's series as a clean name, with the books in series order (`media.metadata.seriesName`,
//! used elsewhere in this app, is a display string with the number glued on: "Name #5"). The
//! server doesn't send the number itself on a real server, so `sequence` is usually `None`; the
//! order the books are stored in is what carries it. The cache
//! this fills (`series_books`) is what the Library groups and orders by, and what Item Detail's
//! "5/8" reads. [`sync_library_series`] runs for every book library as part of the general sync
//! (`sync::sync_all`), best-effort, and Item Detail refreshes it too; [`series_info_for_item`] is
//! a purely local read and never touches the network, which is what makes this offline-friendly
//! after the first fetch: once cached, it's available even without a connection.

use sqlx::SqlitePool;

use abs_storage::repo::series_books::{self, NewSeriesBook};

use crate::error::{CoreError, Result};

/// How many series' numbers are asked for at once.
const NUMBER_REQUESTS_AT_ONCE: usize = 4;

/// Fetches a library's full series list and replaces whatever was cached for it
/// (`repo::series_books::replace_all_for_library`). Malformed entries were already filtered out
/// by `get_library_series_summaries` itself.
///
/// The list has the books of a series in order but not their numbers (a real server sends none
/// there), so each series' numbers are then asked for on their own (`get_series_book_sequences`,
/// one request per series, a few at a time). That part is best effort: a series whose numbers
/// can't be had is still cached with its name and order, just without numbers.
pub async fn sync_library_series(pool: &SqlitePool, api: &abs_api::Client, server_id: &str, library_id: &str) -> Result<()> {
    let series_list = api.get_library_series_summaries(library_id).await.map_err(|e| match e {
        abs_api::LibraryItemsError::Unauthorized(_) => CoreError::Auth,
        other => CoreError::UnexpectedResponse(other.details()),
    })?;

    let numbers = fetch_numbers(api, library_id, &series_list).await;

    let mut rows = Vec::new();
    for series in &series_list {
        for book in &series.books {
            let sequence = book.sequence.as_deref().or_else(|| numbers.get(&(series.id.as_str(), book.item_id.as_str())).map(String::as_str));
            rows.push(NewSeriesBook { series_id: &series.id, series_name: &series.name, item_id: &book.item_id, sequence });
        }
    }

    series_books::replace_all_for_library(pool, server_id, library_id, &rows).await?;
    Ok(())
}

/// The numbers of the books of every series that has a book without one, by (series id, item id).
///
/// Plain loops and `join_all` over a few requests at a time, not stream combinators with
/// closures: those make this future "not general enough" for `Send`, and the sync it belongs to
/// runs on `tokio::spawn`.
async fn fetch_numbers<'a>(
    api: &abs_api::Client,
    library_id: &str,
    series_list: &'a [abs_api::LibrarySeriesSummary],
) -> std::collections::HashMap<(&'a str, &'a str), String> {
    let wanted: Vec<&'a abs_api::LibrarySeriesSummary> = series_list.iter().filter(|series| series.books.iter().any(|book| book.sequence.is_none())).collect();

    let mut numbers = std::collections::HashMap::new();
    for batch in wanted.chunks(NUMBER_REQUESTS_AT_ONCE) {
        let mut requests = Vec::with_capacity(batch.len());
        for series in batch {
            requests.push(api.get_series_book_sequences(library_id, &series.id));
        }
        let answers = futures::future::join_all(requests).await;

        for (series, answer) in batch.iter().zip(answers) {
            let sequences = match answer {
                Ok(sequences) => sequences,
                Err(err) => {
                    tracing::warn!(series = %series.name, error = %err.details(), "couldn't fetch a series' book numbers; keeping its books in order without them");
                    continue;
                }
            };
            for (item_id, sequence) in sequences {
                let Some(sequence) = sequence else { continue };
                // The key borrows the series' own book list, which outlives this call.
                if let Some(book) = series.books.iter().find(|book| book.item_id == item_id) {
                    numbers.insert((series.id.as_str(), book.item_id.as_str()), sequence);
                }
            }
        }
    }
    numbers
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

    /// A real server's series list has the books in order and no numbers; the numbers come from
    /// the items filtered by each series (`filter=series.<base64 id>`).
    async fn mount_numberless_series_list(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/series"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    { "id": "series-1", "name": "Aspirant", "books": [{ "id": "item-1" }, { "id": "item-2" }, { "id": "item-3" }] },
                    { "id": "series-2", "name": "Banda", "books": [{ "id": "item-4" }, { "id": "item-5" }] },
                ]
            })))
            .mount(server)
            .await;
    }

    fn numbered_items(series_id: &str, series_name: &str, numbers: &[(&str, &str)]) -> serde_json::Value {
        let results: Vec<_> = numbers
            .iter()
            .map(|(id, sequence)| serde_json::json!({ "id": id, "media": { "metadata": { "series": { "id": series_id, "name": series_name, "sequence": sequence } } } }))
            .collect();
        serde_json::json!({ "results": results })
    }

    #[tokio::test]
    async fn sync_library_series_fetches_each_seriess_numbers() {
        use wiremock::matchers::query_param;
        let mock_server = MockServer::start().await;
        mount_numberless_series_list(&mock_server).await;
        // base64("series-1") and base64("series-2").
        Mock::given(method("GET")).and(path("/api/libraries/lib-1/items")).and(query_param("filter", "series.c2VyaWVzLTE="))
            .respond_with(ResponseTemplate::new(200).set_body_json(numbered_items("series-1", "Aspirant", &[("item-1", "1"), ("item-2", "2"), ("item-3", "5.5")])))
            .expect(1).mount(&mock_server).await;
        Mock::given(method("GET")).and(path("/api/libraries/lib-1/items")).and(query_param("filter", "series.c2VyaWVzLTI="))
            .respond_with(ResponseTemplate::new(200).set_body_json(numbered_items("series-2", "Banda", &[("item-4", "1"), ("item-5", "2")])))
            .expect(1).mount(&mock_server).await;

        let (pool, server_id) = pool_with_items(&mock_server.uri(), &["item-1", "item-2", "item-3", "item-4", "item-5"]).await;
        sync_library_series(&pool, &abs_api::Client::new(&mock_server.uri()), &server_id, "lib-1").await.unwrap();

        let stored = series_books::list_for_server(&pool, &server_id).await.unwrap();
        let got: Vec<(&str, &str, Option<&str>)> = stored.iter().map(|m| (m.series_name.as_str(), m.item_id.as_str(), m.sequence.as_deref())).collect();
        assert_eq!(
            got,
            [("Aspirant", "item-1", Some("1")), ("Aspirant", "item-2", Some("2")), ("Aspirant", "item-3", Some("5.5")), ("Banda", "item-4", Some("1")), ("Banda", "item-5", Some("2"))]
        );
    }

    /// The numbers are a refinement: when a series' request fails (a 500 here, an older server's
    /// 404 or junk elsewhere), its books are still cached by name and in order, and the other
    /// series still get their numbers.
    #[tokio::test]
    async fn sync_library_series_keeps_names_and_order_when_a_numbers_request_fails() {
        use wiremock::matchers::query_param;
        for failing in [ResponseTemplate::new(500), ResponseTemplate::new(404), ResponseTemplate::new(200).set_body_string("not json")] {
            let mock_server = MockServer::start().await;
            mount_numberless_series_list(&mock_server).await;
            Mock::given(method("GET")).and(path("/api/libraries/lib-1/items")).and(query_param("filter", "series.c2VyaWVzLTE=")).respond_with(failing).mount(&mock_server).await;
            Mock::given(method("GET")).and(path("/api/libraries/lib-1/items")).and(query_param("filter", "series.c2VyaWVzLTI="))
                .respond_with(ResponseTemplate::new(200).set_body_json(numbered_items("series-2", "Banda", &[("item-4", "1"), ("item-5", "2")])))
                .mount(&mock_server).await;

            let (pool, server_id) = pool_with_items(&mock_server.uri(), &["item-1", "item-2", "item-3", "item-4", "item-5"]).await;
            sync_library_series(&pool, &abs_api::Client::new(&mock_server.uri()), &server_id, "lib-1").await.expect("a failing numbers request must not fail the series sync");

            let stored = series_books::list_for_server(&pool, &server_id).await.unwrap();
            let got: Vec<(&str, &str, Option<&str>)> = stored.iter().map(|m| (m.series_name.as_str(), m.item_id.as_str(), m.sequence.as_deref())).collect();
            assert_eq!(
                got,
                [("Aspirant", "item-1", None), ("Aspirant", "item-2", None), ("Aspirant", "item-3", None), ("Banda", "item-4", Some("1")), ("Banda", "item-5", Some("2"))],
                "the series without numbers keeps its books in the server's order"
            );
        }
    }

    /// A server that does send the numbers in the list needs no extra requests.
    #[tokio::test]
    async fn sync_library_series_makes_no_number_requests_when_the_list_has_them() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/libraries/lib-1/series"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [{ "id": "series-1", "name": "Foundation", "books": [{ "id": "item-1", "sequence": "1" }] }] })))
            .mount(&mock_server).await;
        Mock::given(method("GET")).and(path("/api/libraries/lib-1/items")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&mock_server).await;

        let (pool, server_id) = pool_with_items(&mock_server.uri(), &["item-1"]).await;
        sync_library_series(&pool, &abs_api::Client::new(&mock_server.uri()), &server_id, "lib-1").await.unwrap();
        assert_eq!(series_info_for_item(&pool, &server_id, "item-1").await.unwrap().unwrap().sequence.as_deref(), Some("1"));
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
