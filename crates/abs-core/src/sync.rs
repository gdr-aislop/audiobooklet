//! Pulls server state into the local cache.

use abs_storage::repo::items::{self, UpsertItem};
use abs_storage::repo::libraries::{self, UpsertLibrary};
use abs_api::error_chain;
use abs_storage::AppPaths;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use sqlx::SqlitePool;

use crate::error::{CoreError, Result};

/// The one distinction the sync pipeline's error-flattening preserves: a 401/403 from any
/// authenticated call means the stored session itself is dead (revoked refresh token, removed or
/// demoted user, password changed elsewhere) and only signing in again fixes it — the UI keys its
/// "Log in again" affordance off `CoreError::Auth`, and must not show it for a network outage or
/// a 500, which "Try again" can genuinely fix.
fn auth_or_unexpected(status: Option<u16>, fallback: String) -> CoreError {
    match status {
        Some(code) if code == 401 || code == 403 => CoreError::Auth,
        _ => CoreError::UnexpectedResponse(fallback),
    }
}

/// A generated-client call's error as a `CoreError`: `Unreachable` when the server never
/// answered, `Auth` on 401/403, otherwise `UnexpectedResponse`. The string keeps the full error
/// chain either way.
fn api_error<E: std::fmt::Debug + 'static>(err: &abs_api::Error<E>) -> CoreError {
    if abs_api::is_unreachable_error(err) {
        CoreError::Unreachable(error_chain(err))
    } else {
        auth_or_unexpected(err.status().map(|s| s.as_u16()), error_chain(err))
    }
}

/// A library the server listed in the latest sync.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedLibrary {
    pub id: String,
    pub media_type: String,
}

/// Fetch the server's libraries and upsert them into local storage, returning the ones the server
/// listed. Libraries it no longer lists are dealt with by [`sync_all`], which knows which books
/// are downloaded.
pub async fn sync_libraries(pool: &SqlitePool, api: &abs_api::Client, server_id: &str) -> Result<Vec<ListedLibrary>> {
    // `progenitor`'s generated `Error<E>` type differs per operation (its error-body type
    // parameter), so there's no single `From` impl to lean on here — stringify instead,
    // preserving only the auth distinction via the error's HTTP status. The string keeps the
    // full error chain (`error_chain`), so a transport failure is logged/shown with its real
    // cause instead of reqwest's display, which is identical for every failure mode.
    let response = api
        .get_libraries()
        .await
        .map_err(|e| api_error(&e))?;
    let libraries = response.into_inner().libraries;

    let mut listed = Vec::new();
    for library in libraries {
        let Some(id) = library.id else { continue };
        let Some(name) = library.name else { continue };
        let Some(media_type) = library.media_type else { continue };
        let id = id.to_string();

        libraries::upsert(
            pool,
            UpsertLibrary {
                id: &id,
                server_id,
                name: &name,
                media_type: &media_type,
                icon: library.icon.as_deref(),
                display_order: library.display_order.unwrap_or(1),
            },
        )
        .await?;
        listed.push(ListedLibrary { id, media_type });
    }

    Ok(listed)
}

/// Fetch one library's items (with media metadata) and upsert them into local storage, returning
/// how many were synced. Uses `abs_api::Client::get_library_items_with_media` — a hand-written
/// extension, not the generated `get_library_items` — because the generated response type's
/// `results` element (`LibraryItemBase`) has no `media` field at all, a real gap in the vendored
/// spec's schema for this endpoint (see `third_party/audiobookshelf-openapi/README.md`'s "Known
/// gaps"). Items the server no longer lists are left in place here; [`sync_all`] removes them.
pub async fn sync_items_for_library(
    pool: &SqlitePool,
    api: &abs_api::Client,
    server_id: &str,
    library_id: &str,
) -> Result<usize> {
    Ok(store_items_for_library(pool, api, server_id, library_id).await?.len())
}

/// [`sync_items_for_library`], returning the ids the server listed. Items without playable audio
/// (podcasts, e-book-only books) are stored but marked, so no list shows them.
async fn store_items_for_library(
    pool: &SqlitePool,
    api: &abs_api::Client,
    server_id: &str,
    library_id: &str,
) -> Result<HashSet<String>> {
    let remote_items = api
        .get_library_items_with_media(library_id)
        .await
        .map_err(|e| match e {
            abs_api::LibraryItemsError::Unauthorized(_) => CoreError::Auth,
            other if other.is_unreachable() => CoreError::Unreachable(other.details()),
            other => CoreError::UnexpectedResponse(other.details()),
        })?;

    let mut seen = HashSet::new();
    let mut without_audio = Vec::new();
    for item in remote_items {
        let added_at = DateTime::<Utc>::from_timestamp_millis(item.added_at_ms).unwrap_or_else(Utc::now);

        items::upsert(
            pool,
            UpsertItem {
                id: &item.id,
                server_id,
                library_id,
                title: &item.title,
                author: item.author.as_deref(),
                narrator: item.narrator.as_deref(),
                description: item.description.as_deref(),
                duration_seconds: item.duration_seconds,
                added_at,
                series_name: item.series_name.as_deref(),
                genres: &item.genres,
            },
        )
        .await?;
        if !item.has_audio {
            without_audio.push(item.id.clone());
        }
        seen.insert(item.id);
    }
    items::set_without_audio(pool, server_id, library_id, &without_audio).await?;

    Ok(seen)
}

/// Removes a library's stored items the server no longer lists (`listed`), with their cached
/// covers and any partial download — except books with downloaded chapters, which stay playable
/// offline until the user deletes them. Returns how many stayed for that reason.
async fn remove_unlisted_items(
    pool: &SqlitePool,
    paths: &AppPaths,
    server_id: &str,
    library_id: &str,
    listed: &HashSet<String>,
    downloaded: &HashSet<String>,
) -> Result<usize> {
    let mut removed = 0;
    let mut kept = 0;
    for (item_id, _) in items::list_ids_for_library(pool, server_id, library_id).await? {
        if listed.contains(&item_id) {
            continue;
        }
        if downloaded.contains(&item_id) {
            kept += 1;
            continue;
        }
        // Files first: once the row is gone, so are the download rows that name the files.
        crate::download_tracks::clear_item_downloads(pool, server_id, &item_id).await?;
        paths.purge_item_covers(server_id, &item_id).await;
        items::remove(pool, server_id, &item_id).await?;
        removed += 1;
    }
    if removed > 0 {
        tracing::info!(library_id, removed, kept_downloaded = kept, "removed books the server no longer lists");
    }
    Ok(kept)
}

/// One network round-trip's worth of work for a freshly-opened Home screen: sync this server's
/// libraries, then the items of every library the server lists. Builds its own authenticated
/// `abs_api::Client` from `connection` so callers (the `app` crate) never construct one
/// themselves — the same boundary `accounts::add_server_and_login` already draws. `access_token`
/// is the account's stored token; every one of these calls is authenticated (unlike `login`), and
/// minting via `connection` is what actually attaches it *and* honors the server's connection
/// settings — a plain `Client::new` sends no auth at all and every call comes back `401` (caught
/// live against the real demo server, not just in theory).
///
/// The device follows the server: books and libraries the server no longer lists (deleted, or
/// no longer shared with this user) are removed, except books with downloaded chapters, which
/// stay until the user deletes them. A library kept only for such books is marked
/// `removed_on_server`, and its items are never asked for again — asking used to fail every sync
/// with a 404 once a library was deleted on the server. Podcast libraries are listed but their
/// shows aren't fetched: podcasts aren't supported yet.
pub async fn sync_all(
    pool: &SqlitePool,
    connection: &crate::connection::ConnectionTarget,
    paths: &AppPaths,
    server_id: &str,
    access_token: &str,
) -> Result<()> {
    let api = connection
        .api_client(access_token)
        .map_err(|e| CoreError::UnexpectedResponse(error_chain(&e)))?;
    let listed = sync_libraries(pool, &api, server_id).await?;
    let downloaded: HashSet<String> =
        abs_storage::repo::download_tracks::downloaded_item_ids(pool, server_id).await?.into_iter().collect();

    for library in &listed {
        let items_listed = if library.media_type == "podcast" {
            HashSet::new()
        } else {
            store_items_for_library(pool, &api, server_id, &library.id).await?
        };
        remove_unlisted_items(pool, paths, server_id, &library.id, &items_listed, &downloaded).await?;
    }

    for library in libraries::list_for_server(pool, server_id).await? {
        if listed.iter().any(|l| l.id == library.id) {
            continue;
        }
        let kept = remove_unlisted_items(pool, paths, server_id, &library.id, &HashSet::new(), &downloaded).await?;
        if kept > 0 {
            libraries::set_removed_on_server(pool, server_id, &library.id).await?;
        } else {
            libraries::remove(pool, server_id, &library.id).await?;
            tracing::info!(library_id = %library.id, "removed a library the server no longer lists");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ConnectionTarget;
    use abs_storage::connect_and_migrate;
    use abs_storage::repo::servers;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn pool_with_server(server_url: &str) -> (SqlitePool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = servers::add(&pool, server_url).await.unwrap();
        (pool, server_id)
    }

    /// Item rows have a foreign key on `library_id`, so item-sync tests need a real library row
    /// to attach to first — `sync_items_for_library` itself never creates one (that's
    /// `sync_libraries`'/`sync_all`'s job).
    async fn pool_with_server_and_library(server_url: &str, library_id: &str) -> (SqlitePool, String) {
        let (pool, server_id) = pool_with_server(server_url).await;
        libraries::upsert(
            &pool,
            UpsertLibrary {
                id: library_id,
                server_id: &server_id,
                name: "Audiobooks",
                media_type: "book",
                icon: None,
                display_order: 1,
            },
        )
        .await
        .unwrap();
        (pool, server_id)
    }

    #[tokio::test]
    async fn sync_libraries_upserts_every_returned_library() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraries": [
                    {
                        "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b",
                        "name": "Audiobooks",
                        "mediaType": "book",
                        "icon": "audiobookshelf",
                        "displayOrder": 1,
                    },
                    {
                        "id": "b2a1e9c0-4a4f-4dd6-8be0-e615d233185c",
                        "name": "Podcasts",
                        "mediaType": "podcast",
                        "icon": "podcast",
                        "displayOrder": 2,
                    },
                ]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server(&mock_server.uri()).await;
        let api = abs_api::Client::new(&mock_server.uri());

        let synced = sync_libraries(&pool, &api, &server_id).await.unwrap();
        assert_eq!(synced.len(), 2);

        let stored = libraries::list_for_server(&pool, &server_id).await.unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].name, "Audiobooks");
        assert_eq!(stored[1].name, "Podcasts");
    }

    #[tokio::test]
    async fn sync_libraries_running_twice_does_not_duplicate() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraries": [{
                    "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b",
                    "name": "Audiobooks",
                    "mediaType": "book",
                    "displayOrder": 1,
                }]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server(&mock_server.uri()).await;
        let api = abs_api::Client::new(&mock_server.uri());

        sync_libraries(&pool, &api, &server_id).await.unwrap();
        sync_libraries(&pool, &api, &server_id).await.unwrap();

        assert_eq!(libraries::list_for_server(&pool, &server_id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sync_libraries_skips_entries_missing_required_fields() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraries": [
                    { "name": "No ID", "mediaType": "book" },
                    {
                        "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b",
                        "name": "Valid Library",
                        "mediaType": "book",
                    },
                ]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server(&mock_server.uri()).await;
        let api = abs_api::Client::new(&mock_server.uri());

        let synced = sync_libraries(&pool, &api, &server_id).await.unwrap();
        assert_eq!(synced.len(), 1, "only the well-formed library should be synced");
    }

    #[tokio::test]
    async fn sync_libraries_propagates_server_errors() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server(&mock_server.uri()).await;
        let api = abs_api::Client::new(&mock_server.uri());

        let result = sync_libraries(&pool, &api, &server_id).await;
        assert!(
            matches!(result, Err(CoreError::UnexpectedResponse(_))),
            "a 500 is a server problem, not an auth failure — the UI must not offer 'Log in again' for it"
        );
    }

    /// A URL on this machine that nothing listens on: connecting to it is refused at once.
    fn closed_port_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        url
    }

    /// The server never answering is told apart from it answering badly, so the UI can say
    /// "can't reach your server" instead of blaming the server's response.
    #[tokio::test]
    async fn a_refused_connection_is_unreachable_not_an_unexpected_response() {
        let url = closed_port_url();
        let (pool, server_id) = pool_with_server_and_library(&url, "lib-1").await;
        let api = abs_api::Client::new(&url);

        let libraries = sync_libraries(&pool, &api, &server_id).await;
        assert!(matches!(libraries, Err(CoreError::Unreachable(_))), "libraries: {libraries:?}");
        let items = sync_items_for_library(&pool, &api, &server_id, "lib-1").await;
        assert!(matches!(items, Err(CoreError::Unreachable(_))), "items: {items:?}");
    }

    /// The classification the re-login flow is built on: a 401 from the libraries call means the
    /// stored session is dead, and `CoreError::Auth` is what the UI keys its "Log in again"
    /// affordance off.
    #[tokio::test]
    async fn sync_libraries_maps_a_401_to_the_auth_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server(&mock_server.uri()).await;
        let api = abs_api::Client::new(&mock_server.uri());

        let result = sync_libraries(&pool, &api, &server_id).await;
        assert!(
            matches!(result, Err(CoreError::Auth)),
            "a 401 must be classified as an auth failure, not a generic one"
        );
    }

    /// Same classification, hand-written-op side: `get_library_items_with_media` carries the
    /// status in its own `LibraryItemsError`, and the sync layer must map it to `CoreError::Auth`
    /// too — otherwise a session that dies mid-sync (after libraries, on items) would look like a
    /// generic failure.
    #[tokio::test]
    async fn sync_items_for_library_maps_a_401_to_the_auth_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "lib-1").await;
        let api = abs_api::Client::new(&mock_server.uri());

        let result = sync_items_for_library(&pool, &api, &server_id, "lib-1").await;
        assert!(matches!(result, Err(CoreError::Auth)));
    }

    #[tokio::test]
    async fn sync_libraries_defaults_missing_display_order_to_one() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraries": [{
                    "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b",
                    "name": "Audiobooks",
                    "mediaType": "book",
                }]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server(&mock_server.uri()).await;
        let api = abs_api::Client::new(&mock_server.uri());

        sync_libraries(&pool, &api, &server_id).await.unwrap();

        let stored = libraries::list_for_server(&pool, &server_id).await.unwrap();
        assert_eq!(stored[0].display_order, 1);
    }

    fn item_json(id: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "addedAt": 1_700_000_000_000i64,
            "media": {
                "duration": 3600.0,
                "metadata": { "title": title, "authorName": "Andy Weir" }
            }
        })
    }

    #[tokio::test]
    async fn sync_items_for_library_upserts_every_returned_item() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [item_json("item-1", "Project Hail Mary"), item_json("item-2", "Dune")]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "lib-1").await;
        let api = abs_api::Client::new(&mock_server.uri());

        let synced = sync_items_for_library(&pool, &api, &server_id, "lib-1").await.unwrap();
        assert_eq!(synced, 2);

        let stored = items::list_for_library(&pool, &server_id, "lib-1").await.unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].author.as_deref(), Some("Andy Weir"));
    }

    #[tokio::test]
    async fn sync_items_for_library_stores_series_and_genres() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "item-1",
                    "addedAt": 1_700_000_000_000i64,
                    "media": {
                        "duration": 3600.0,
                        "metadata": {
                            "title": "Wizard's First Rule",
                            "seriesName": "Sword of Truth",
                            "genres": ["Fantasy", "Sci-Fi"],
                        }
                    }
                }]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "lib-1").await;
        let api = abs_api::Client::new(&mock_server.uri());
        sync_items_for_library(&pool, &api, &server_id, "lib-1").await.unwrap();

        let stored = items::list_for_library(&pool, &server_id, "lib-1").await.unwrap();
        assert_eq!(stored[0].series_name.as_deref(), Some("Sword of Truth"));
        assert_eq!(stored[0].genres(), vec!["Fantasy".to_string(), "Sci-Fi".to_string()]);
    }

    #[tokio::test]
    async fn sync_items_for_library_running_twice_does_not_duplicate() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [item_json("item-1", "Project Hail Mary")]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "lib-1").await;
        let api = abs_api::Client::new(&mock_server.uri());

        sync_items_for_library(&pool, &api, &server_id, "lib-1").await.unwrap();
        sync_items_for_library(&pool, &api, &server_id, "lib-1").await.unwrap();

        let stored = items::list_for_library(&pool, &server_id, "lib-1").await.unwrap();
        assert_eq!(stored.len(), 1);
    }

    #[tokio::test]
    async fn sync_items_for_library_skips_entries_missing_media_or_title() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    { "id": "no-media" },
                    item_json("item-1", "Valid Item"),
                ]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "lib-1").await;
        let api = abs_api::Client::new(&mock_server.uri());

        let synced = sync_items_for_library(&pool, &api, &server_id, "lib-1").await.unwrap();
        assert_eq!(synced, 1, "only the well-formed item should be synced");
    }

    #[tokio::test]
    async fn sync_items_for_library_propagates_server_errors() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "lib-1").await;
        let api = abs_api::Client::new(&mock_server.uri());

        let result = sync_items_for_library(&pool, &api, &server_id, "lib-1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sync_items_for_library_defaults_missing_duration_to_zero() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "item-1",
                    "media": { "metadata": { "title": "No Duration" } }
                }]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "lib-1").await;
        let api = abs_api::Client::new(&mock_server.uri());

        sync_items_for_library(&pool, &api, &server_id, "lib-1").await.unwrap();

        let stored = items::list_for_library(&pool, &server_id, "lib-1").await.unwrap();
        assert_eq!(stored[0].duration_seconds, 0.0);
    }

    fn test_paths() -> AppPaths {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path().join("data"), tmp.path().join("cache"), tmp.path().join("state"));
        std::mem::forget(tmp);
        paths
    }

    async fn mount_libraries(server: &MockServer, libraries: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "libraries": libraries })))
            .mount(server)
            .await;
    }

    async fn mount_items(server: &MockServer, library_id: &str, results: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(format!("/api/libraries/{library_id}/items")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": results })))
            .mount(server)
            .await;
    }

    /// A completely downloaded one-track book, as a finished download leaves it.
    async fn mark_downloaded(pool: &SqlitePool, server_id: &str, item_id: &str) {
        let track = abs_storage::repo::tracks::NewTrack { ino: "ino-1", duration_seconds: 10.0, offset_seconds: 0.0, size_bytes: None };
        abs_storage::repo::tracks::upsert_all(pool, server_id, item_id, &[track]).await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(pool, server_id, item_id, "ino-1", "/nowhere/ino-1.mp3").await.unwrap();
        abs_storage::repo::download_tracks::mark_complete(pool, server_id, item_id, "ino-1", 10).await.unwrap();
    }

    async fn stored_titles(pool: &SqlitePool, server_id: &str, library_id: &str) -> Vec<String> {
        let mut titles: Vec<String> =
            items::list_for_library(pool, server_id, library_id).await.unwrap().into_iter().map(|i| i.title).collect();
        titles.sort();
        titles
    }

    #[tokio::test]
    async fn sync_all_syncs_libraries_then_every_librarys_items() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraries": [
                    { "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" },
                    { "id": "f1c2e9c0-4a4f-4dd6-8be0-e615d233185d", "name": "Kids", "mediaType": "book" },
                ]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [item_json("item-1", "Project Hail Mary")]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/f1c2e9c0-4a4f-4dd6-8be0-e615d233185d/items"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [item_json("item-2", "The Little Red Tractor")]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id) = pool_with_server(&mock_server.uri()).await;

        // Regression coverage: `sync_all` used to build an unauthenticated `abs_api::Client`,
        // which every one of these mocks (each requiring the Authorization header) would reject —
        // caught live against the real demo server as a blanket 401, not just here.
        sync_all(&pool, &ConnectionTarget::direct(&mock_server.uri()), &test_paths(), &server_id, "test-token").await.unwrap();

        assert_eq!(libraries::list_for_server(&pool, &server_id).await.unwrap().len(), 2);
        assert_eq!(stored_titles(&pool, &server_id, "e4bb1afb-4a4f-4dd6-8be0-e615d233185b").await, ["Project Hail Mary"]);
        assert_eq!(stored_titles(&pool, &server_id, "f1c2e9c0-4a4f-4dd6-8be0-e615d233185d").await, ["The Little Red Tractor"]);
    }

    /// Found in the 2026-10-12 manual pass: once a library was deleted on the server, every sync
    /// asked for its items, got a 404 and failed — for good, on every launch.
    #[tokio::test]
    async fn a_library_deleted_on_the_server_is_removed_and_sync_keeps_working() {
        let mock_server = MockServer::start().await;
        mount_libraries(&mock_server, serde_json::json!([{ "id": "11111111-1111-4111-8111-111111111111", "name": "Audiobooks", "mediaType": "book" }])).await;
        mount_items(&mock_server, "11111111-1111-4111-8111-111111111111", serde_json::json!([item_json("item-1", "Project Hail Mary")])).await;
        Mock::given(method("GET")).and(path("/api/libraries/22222222-2222-4222-8222-222222222222/items")).respond_with(ResponseTemplate::new(404)).expect(0).mount(&mock_server).await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "22222222-2222-4222-8222-222222222222").await;
        items::upsert(&pool, UpsertItem { id: "old-item", server_id: &server_id, library_id: "22222222-2222-4222-8222-222222222222", title: "Late Arrival", author: None, narrator: None, description: None, duration_seconds: 90.0, added_at: Utc::now(), series_name: None, genres: &[] }).await.unwrap();
        let paths = test_paths();
        let cover = paths.cover_cache_path(&server_id, "old-item", "jpg");
        tokio::fs::create_dir_all(cover.parent().unwrap()).await.unwrap();
        tokio::fs::write(&cover, b"cover").await.unwrap();

        sync_all(&pool, &ConnectionTarget::direct(&mock_server.uri()), &paths, &server_id, "token").await.unwrap();

        let stored: Vec<String> = libraries::list_for_server(&pool, &server_id).await.unwrap().into_iter().map(|l| l.id).collect();
        assert_eq!(stored, ["11111111-1111-4111-8111-111111111111"], "the deleted library is gone");
        assert!(items::get(&pool, &server_id, "old-item").await.is_err(), "and its book with it");
        assert!(!cover.exists(), "the book's cached cover is deleted too");
        sync_all(&pool, &ConnectionTarget::direct(&mock_server.uri()), &paths, &server_id, "token").await.unwrap();
    }

    /// Downloaded books outlive their removal on the server, so they stay playable offline; the
    /// library holding them is kept, marked, and never asked for again.
    #[tokio::test]
    async fn downloaded_books_are_kept_when_the_server_drops_them() {
        let mock_server = MockServer::start().await;
        mount_libraries(&mock_server, serde_json::json!([{ "id": "11111111-1111-4111-8111-111111111111", "name": "Audiobooks", "mediaType": "book" }])).await;
        mount_items(&mock_server, "11111111-1111-4111-8111-111111111111", serde_json::json!([item_json("still-there", "Still There")])).await;
        Mock::given(method("GET")).and(path("/api/libraries/22222222-2222-4222-8222-222222222222/items")).respond_with(ResponseTemplate::new(404)).expect(0).mount(&mock_server).await;

        let (pool, server_id) = pool_with_server_and_library(&mock_server.uri(), "11111111-1111-4111-8111-111111111111").await;
        libraries::upsert(&pool, UpsertLibrary { id: "22222222-2222-4222-8222-222222222222", server_id: &server_id, name: "Extra", media_type: "book", icon: None, display_order: 2 }).await.unwrap();
        let book = |id: &'static str, library_id: &'static str| UpsertItem { id, server_id: &server_id, library_id, title: id, author: None, narrator: None, description: None, duration_seconds: 90.0, added_at: Utc::now(), series_name: None, genres: &[] };
        for (id, library) in [("still-there", "11111111-1111-4111-8111-111111111111"), ("deleted-downloaded", "11111111-1111-4111-8111-111111111111"), ("deleted", "11111111-1111-4111-8111-111111111111"), ("in-gone-downloaded", "22222222-2222-4222-8222-222222222222"), ("in-gone", "22222222-2222-4222-8222-222222222222")] {
            items::upsert(&pool, book(id, library)).await.unwrap();
        }
        mark_downloaded(&pool, &server_id, "deleted-downloaded").await;
        mark_downloaded(&pool, &server_id, "in-gone-downloaded").await;

        sync_all(&pool, &ConnectionTarget::direct(&mock_server.uri()), &test_paths(), &server_id, "token").await.unwrap();

        assert_eq!(stored_titles(&pool, &server_id, "11111111-1111-4111-8111-111111111111").await, ["Still There", "deleted-downloaded"]);
        assert_eq!(stored_titles(&pool, &server_id, "22222222-2222-4222-8222-222222222222").await, ["in-gone-downloaded"]);
        let gone = libraries::get(&pool, &server_id, "22222222-2222-4222-8222-222222222222").await.unwrap();
        assert!(gone.removed_on_server, "kept only for its download, and marked so");
        assert!(!libraries::get(&pool, &server_id, "11111111-1111-4111-8111-111111111111").await.unwrap().removed_on_server);

        // Back on the server: the flag clears.
        mock_server.reset().await;
        mount_libraries(&mock_server, serde_json::json!([{ "id": "11111111-1111-4111-8111-111111111111", "name": "Audiobooks", "mediaType": "book" }, { "id": "22222222-2222-4222-8222-222222222222", "name": "Extra", "mediaType": "book" }])).await;
        mount_items(&mock_server, "11111111-1111-4111-8111-111111111111", serde_json::json!([item_json("still-there", "Still There")])).await;
        mount_items(&mock_server, "22222222-2222-4222-8222-222222222222", serde_json::json!([item_json("in-gone-downloaded", "in-gone-downloaded")])).await;
        sync_all(&pool, &ConnectionTarget::direct(&mock_server.uri()), &test_paths(), &server_id, "token").await.unwrap();
        assert!(!libraries::get(&pool, &server_id, "22222222-2222-4222-8222-222222222222").await.unwrap().removed_on_server);
    }

    /// Podcasts (episodes) and e-book-only books have nothing this app can play: they're kept
    /// out of every list, and a podcast library's shows aren't even fetched.
    #[tokio::test]
    async fn podcasts_and_ebooks_are_left_out_of_the_lists() {
        let mock_server = MockServer::start().await;
        mount_libraries(&mock_server, serde_json::json!([
            { "id": "33333333-3333-4333-8333-333333333333", "name": "Audiobooks", "mediaType": "book" },
            { "id": "44444444-4444-4444-8444-444444444444", "name": "Podcasts", "mediaType": "podcast" },
        ])).await;
        mount_items(&mock_server, "33333333-3333-4333-8333-333333333333", serde_json::json!([
            { "id": "audio", "mediaType": "book", "media": { "duration": 60.0, "numAudioFiles": 1, "numTracks": 1, "metadata": { "title": "Audiobook" } } },
            { "id": "ebook", "mediaType": "book", "media": { "duration": 0, "numAudioFiles": 0, "numTracks": 0, "ebookFormat": "epub", "metadata": { "title": "E-book" } } },
            item_json("no-counts", "Older Server Book"),
        ])).await;
        Mock::given(method("GET")).and(path("/api/libraries/44444444-4444-4444-8444-444444444444/items")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&mock_server).await;

        let (pool, server_id) = pool_with_server(&mock_server.uri()).await;
        sync_all(&pool, &ConnectionTarget::direct(&mock_server.uri()), &test_paths(), &server_id, "token").await.unwrap();

        assert_eq!(stored_titles(&pool, &server_id, "33333333-3333-4333-8333-333333333333").await, ["Audiobook", "Older Server Book"]);
        assert!(items::get(&pool, &server_id, "ebook").await.is_ok(), "stored, just not listed");
        assert_eq!(libraries::list_for_server(&pool, &server_id).await.unwrap().len(), 2, "the podcast library itself is still listed");
    }
}
