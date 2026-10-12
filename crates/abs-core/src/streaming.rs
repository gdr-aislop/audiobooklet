//! Resolves playable URLs for an item's audio, for the Player screen. `abs-api` has no coverage
//! for `/api/items/*` at all (see `third_party/audiobookshelf-openapi/README.md`'s "Known gaps"),
//! so `abs_api::Client::get_item_playback_info` is a hand-written extension; this module is the
//! one place that turns its result into something `abs-player` can actually load. Items split
//! across multiple audio files get one `StreamTrack` per file, each with its own URL — the caller
//! plays them in sequence.

use crate::error::{CoreError, Result};

pub struct StreamTarget {
    /// One entry per audio file, in book order. A book split across multiple files is played by
    /// loading these one after another — see `StreamTrack::offset_seconds` for how callers map
    /// book-level positions onto individual files.
    pub tracks: Vec<StreamTrack>,
    /// The book-level duration: the sum of every track's duration, not any single file's.
    pub duration_seconds: f64,
    /// The item's chapters, if any — comes free in the same `GET /api/items/:id` response used to
    /// resolve the audio files, so no second network call is needed to get this. Chapter
    /// positions are book-level, spanning across file boundaries.
    pub chapters: Vec<abs_api::ChapterRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamTrack {
    pub ino: String,
    pub url: String,
    pub duration_seconds: f64,
    /// Where this file starts within the whole book, computed from the server-reported durations
    /// of the files before it. Progress and chapters are defined book-level, so everything
    /// user-facing positions itself through this.
    pub offset_seconds: f64,
    /// The server-reported file size, when the item's metadata carries it — what download size
    /// estimates are computed from. `None` is "unknown", never an error.
    pub size_bytes: Option<u64>,
}

/// Resolves a playable stream for an item through the server connection `connection` describes
/// (base URL after local-address resolution, plus the connection's transport options) — the
/// same boundary `abs_core::sync::sync_all` already draws, so callers (the `app` crate) never
/// construct a client themselves.
pub async fn resolve_stream_target(connection: &crate::connection::ConnectionTarget, access_token: &str, item_id: &str) -> Result<StreamTarget> {
    let api = connection
        .api_client(access_token)
        .map_err(|e| CoreError::UnexpectedResponse(e.to_string()))?;
    let info = api.get_item_playback_info(item_id).await.map_err(|e| match e {
        // Distinguished so callers (e.g. `app::player::PlayerController::start`) can show
        // "session expired — log in again" rather than a generic network failure — the same
        // 401/403-means-Auth convention `sync.rs`'s `auth_or_unexpected` already draws.
        abs_api::LibraryItemsError::Unauthorized(_) => CoreError::Auth,
        other if other.is_unreachable() => CoreError::Unreachable(other.details()),
        other => CoreError::UnexpectedResponse(other.details()),
    })?;

    if info.audio_files.is_empty() {
        return Err(CoreError::NoPlayableAudio(item_id.to_string()));
    }

    let mut tracks = Vec::with_capacity(info.audio_files.len());
    let mut offset_seconds = 0.0;
    for file in &info.audio_files {
        tracks.push(StreamTrack {
            ino: file.ino.clone(),
            url: connection.track_url(item_id, &file.ino, access_token),
            duration_seconds: file.duration_seconds,
            offset_seconds,
            size_bytes: file.size_bytes,
        });
        offset_seconds += file.duration_seconds;
    }

    Ok(StreamTarget { tracks, duration_seconds: offset_seconds, chapters: info.chapters })
}

/// Finds which track a book-level position falls into, and how far into that track it is.
/// `rposition` (not `position`) matters at an exact track boundary: a position that lands exactly
/// on the start of a later track must resolve to that later track, not linger on the end of the
/// previous one. Shared by playback (mapping the resume position and seek targets onto the
/// currently-loaded track) and downloads (mapping a chapter's `[start, end)` range onto the set of
/// tracks it touches) — one definition, tested once. With no tracks at all (an item whose start
/// failed before anything resolved) this is `(0, 0.0)` rather than a panic.
pub fn locate_track(tracks: &[StreamTrack], book_seconds: f64) -> (usize, f64) {
    if tracks.is_empty() {
        return (0, 0.0);
    }
    let index = tracks.iter().rposition(|t| t.offset_seconds <= book_seconds + 1e-6).unwrap_or(0);
    (index, (book_seconds - tracks[index].offset_seconds).max(0.0))
}

/// Builds a [`StreamTarget`] entirely from locally cached state — the offline fallback for
/// callers whose `resolve_stream_target` failed. Tracks come from `tracks::cached_tracks`
/// (synced as a side effect of every resolve and download run), chapters from
/// `chapters::cached_chapters`. No download-row check gates this: the local-file-vs-stream
/// preference happens per track at load time (`download_tracks::local_track_path`, via the
/// caller's own URL resolution), not here — a not-yet-downloaded track simply gets the normal
/// streaming URL, which fails to load when the server is genuinely unreachable, stopping
/// playback at that gap through the caller's existing error handling. So a fully-downloaded
/// item plays end-to-end offline and a partially-downloaded one plays until the first missing
/// chapter. Err means there's no cached track metadata at all — the item was never resolved or
/// downloaded on this device — and playback genuinely cannot start.
pub async fn offline_stream_target(
    pool: &sqlx::SqlitePool,
    server_id: &str,
    item_id: &str,
    connection: &crate::connection::ConnectionTarget,
    access_token: &str,
) -> Result<StreamTarget> {
    let cached = crate::tracks::cached_tracks(pool, server_id, item_id).await?;
    if cached.is_empty() {
        return Err(CoreError::UnexpectedResponse(format!("item {item_id} has no cached track metadata to play from")));
    }
    let mut tracks = Vec::with_capacity(cached.len());
    let mut duration_seconds = 0.0;
    for track in &cached {
        tracks.push(StreamTrack {
            ino: track.ino.clone(),
            url: connection.track_url(item_id, &track.ino, access_token),
            duration_seconds: track.duration_seconds,
            offset_seconds: track.offset_seconds,
            size_bytes: track.size_bytes,
        });
        duration_seconds += track.duration_seconds;
    }
    let chapters = crate::chapters::cached_chapters(pool, server_id, item_id).await?;
    Ok(StreamTarget { tracks, duration_seconds, chapters })
}

/// Pushes local playback progress up to the server, so it shows up in the official apps and
/// survives a fresh install — not just recorded in this client's own local `progress` table.
/// Callers are expected to treat a failure here as non-fatal: the local write (the source of
/// truth for this client's own "Continue Listening") already happened by the time this runs, and
/// a transient network failure syncing it up shouldn't be surfaced as a playback error.
pub async fn sync_progress_to_server(
    connection: &crate::connection::ConnectionTarget,
    access_token: &str,
    item_id: &str,
    current_time_seconds: f64,
    duration_seconds: f64,
    is_finished: bool,
) -> Result<()> {
    let api = connection
        .api_client(access_token)
        .map_err(|e| CoreError::UnexpectedResponse(e.to_string()))?;
    sync_progress_to_server_with_client(&api, item_id, current_time_seconds, duration_seconds, is_finished).await
}

/// Same as [`sync_progress_to_server`], but against an already-minted client instead of building
/// one from a connection + token — for callers that maintain their own client cache
/// (`abs_core::auth::Session::api_client`) rather than minting fresh on every call. The player's
/// periodic progress sync is the motivating (and, as of this writing, only) caller: it runs often
/// enough while playing that minting a fresh `reqwest::Client` — a full TLS config build, then a
/// fresh handshake on first use — every single time was real, avoidable overhead.
pub async fn sync_progress_to_server_with_client(
    api: &abs_api::Client,
    item_id: &str,
    current_time_seconds: f64,
    duration_seconds: f64,
    is_finished: bool,
) -> Result<()> {
    api.update_media_progress(item_id, current_time_seconds, duration_seconds, is_finished)
        .await
        .map_err(|e| match e {
            abs_api::LibraryItemsError::Unauthorized(_) => CoreError::Auth,
            abs_api::LibraryItemsError::NotFound(what) => CoreError::NotOnServer(what),
            other => CoreError::UnexpectedResponse(other.details()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ConnectionTarget;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn resolving_against_a_server_that_refuses_connections_is_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let result = resolve_stream_target(&ConnectionTarget::direct(&url), "test-token", "item-1").await;
        assert!(matches!(result, Err(CoreError::Unreachable(_))), "got {:?}", result.err());
    }

    #[tokio::test]
    async fn resolve_stream_target_builds_the_expected_url() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [{ "ino": "12345", "duration": 3600.0 }] }
            })))
            .mount(&mock_server)
            .await;

        let target = resolve_stream_target(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1").await.unwrap();

        assert_eq!(target.tracks.len(), 1);
        assert_eq!(
            target.tracks[0].url,
            format!("{}/api/items/item-1/file/12345?token=test-token", mock_server.uri())
        );
        assert_eq!(target.tracks[0].duration_seconds, 3600.0);
        assert_eq!(target.tracks[0].offset_seconds, 0.0);
        assert_eq!(target.duration_seconds, 3600.0);
        assert!(target.chapters.is_empty());
    }

    #[tokio::test]
    async fn resolve_stream_target_includes_chapters() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [{ "ino": "12345", "duration": 3600.0 }],
                    "chapters": [
                        { "id": 0, "start": 0.0, "end": 1800.0, "title": "Part One" },
                        { "id": 1, "start": 1800.0, "end": 3600.0, "title": "Part Two" },
                    ]
                }
            })))
            .mount(&mock_server)
            .await;

        let target = resolve_stream_target(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1").await.unwrap();

        assert_eq!(target.chapters.len(), 2);
        assert_eq!(target.chapters[0].title, "Part One");
        assert_eq!(target.chapters[1].start_seconds, 1800.0);
    }

    #[tokio::test]
    async fn resolve_stream_target_lists_every_track_with_its_offset() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [
                        { "ino": "111", "duration": 1800.0 },
                        { "ino": "222", "duration": 1800.0 },
                    ]
                }
            })))
            .mount(&mock_server)
            .await;

        let target = resolve_stream_target(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1").await.unwrap();

        assert_eq!(target.tracks.len(), 2, "should list every audio file, not just the first");
        assert!(target.tracks[0].url.contains("/file/111"), "first track's URL: {}", target.tracks[0].url);
        assert!(target.tracks[1].url.contains("/file/222"), "second track's URL: {}", target.tracks[1].url);
        assert_eq!(target.tracks[0].offset_seconds, 0.0);
        assert_eq!(target.tracks[1].offset_seconds, 1800.0, "the second track starts where the first ends");
        assert_eq!(target.duration_seconds, 3600.0, "book-level duration spans every track");
    }

    /// A 401 must come back as `CoreError::Auth`, not a generic `UnexpectedResponse` — this is
    /// what lets `app::player::PlayerController::start` show "log in again" instead of a plain
    /// network-failure message for the most common real trigger (an expired session).
    #[tokio::test]
    async fn resolve_stream_target_maps_401_to_auth() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/items/item-1")).respond_with(ResponseTemplate::new(401)).mount(&mock_server).await;

        let result = resolve_stream_target(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1").await;

        assert!(matches!(result, Err(CoreError::Auth)), "a 401 must map to CoreError::Auth");
    }

    #[tokio::test]
    async fn resolve_stream_target_with_no_audio_files_is_an_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [] }
            })))
            .mount(&mock_server)
            .await;

        let result = resolve_stream_target(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1").await;
        assert!(
            matches!(&result, Err(CoreError::NoPlayableAudio(id)) if id == "item-1"),
            "an item with no audio files at all can't be played, and says so: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn resolve_stream_target_propagates_server_errors() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let result = resolve_stream_target(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1").await;
        assert!(result.is_err());
    }

    /// Track URLs are built from the connection's *resolved* base URL — when the server's
    /// local network address is configured and probed reachable, playback streams from the LAN
    /// address, since that's the URL GStreamer will actually load.
    #[test]
    fn track_urls_follow_the_resolved_base_url() {
        let row = abs_storage::models::Server {
            url: "https://remote.example.org".to_string(),
            local_network_address: Some("http://192.168.1.50:13378".to_string()),
            ..server_row_for("https://remote.example.org")
        };
        let connection = ConnectionTarget::resolve(&row, Some(true));
        assert_eq!(
            connection.track_url("item-1", "12345", "t"),
            "http://192.168.1.50:13378/api/items/item-1/file/12345?token=t"
        );
    }

    /// A minimal `Server` row for connection-resolution tests.
    fn server_row_for(url: &str) -> abs_storage::models::Server {
        abs_storage::models::Server {
            id: "server-1".to_string(),
            url: url.to_string(),
            custom_headers_json: "{}".to_string(),
            disable_ssl_verify: false,
            client_cert_path: None,
            client_cert_password: None,
            local_network_address: None,
            user_agent: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn sync_progress_to_server_sends_the_expected_request() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/me/progress/item-1"))
            .and(body_partial_json(serde_json::json!({
                "currentTime": 42.5,
                "duration": 100.0,
                "isFinished": false,
            })))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        sync_progress_to_server(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1", 42.5, 100.0, false).await.unwrap();
    }

    /// Same shape `tracks.rs`'s own tests need: a migrated pool plus the server/library/item rows
    /// the `tracks` table's foreign keys require.
    async fn pool_with_synced_item() -> (sqlx::SqlitePool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = abs_storage::repo::servers::add(&pool, "https://a.example").await.unwrap();
        abs_storage::repo::libraries::upsert(
            &pool,
            abs_storage::repo::libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        abs_storage::repo::items::upsert(
            &pool,
            abs_storage::repo::items::UpsertItem {
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
        (pool, server_id)
    }

    #[tokio::test]
    async fn offline_stream_target_builds_from_cached_tracks_and_chapters() {
        let (pool, server_id) = pool_with_synced_item().await;
        let tracks = two_tracks();
        crate::tracks::sync_item_tracks(&pool, &server_id, "item-1", &tracks).await.unwrap();
        crate::chapters::sync_item_chapters(
            &pool,
            &server_id,
            "item-1",
            &[abs_api::ChapterRef { title: "Part One".into(), start_seconds: 0.0, end_seconds: 1800.0 }],
        )
        .await
        .unwrap();

        // Deliberately no download rows: the offline target is built from cached *metadata*
        // alone — whether a file is actually on disk is decided per track at load time, not here.
        let target = offline_stream_target(&pool, &server_id, "item-1", &ConnectionTarget::direct("https://a.example"), "tok").await.unwrap();

        assert_eq!(target.tracks.len(), 2);
        assert_eq!(target.tracks[0].ino, "1");
        assert_eq!(target.tracks[1].ino, "2");
        assert_eq!(target.tracks[1].offset_seconds, 1800.0);
        assert_eq!(target.tracks[1].url, "https://a.example/api/items/item-1/file/2?token=tok");
        assert_eq!(target.duration_seconds, 3600.0);
        assert_eq!(target.chapters.len(), 1);
        assert_eq!(target.chapters[0].title, "Part One");
    }

    #[tokio::test]
    async fn offline_stream_target_errors_without_cached_tracks() {
        let (pool, server_id) = pool_with_synced_item().await;

        let result = offline_stream_target(&pool, &server_id, "item-1", &ConnectionTarget::direct("https://a.example"), "tok").await;

        assert!(result.is_err(), "an item never resolved or downloaded on this device can't be played offline");
    }

    fn two_tracks() -> Vec<StreamTrack> {
        vec![
            StreamTrack { ino: "1".into(), url: "u1".into(), duration_seconds: 1800.0, offset_seconds: 0.0, size_bytes: Some(1) },
            StreamTrack { ino: "2".into(), url: "u2".into(), duration_seconds: 1800.0, offset_seconds: 1800.0, size_bytes: Some(2) },
        ]
    }

    #[test]
    fn locate_track_finds_the_containing_track() {
        assert_eq!(locate_track(&two_tracks(), 900.0), (0, 900.0));
        assert_eq!(locate_track(&two_tracks(), 2000.0), (1, 200.0));
    }

    #[test]
    fn locate_track_at_an_exact_boundary_resolves_to_the_later_track() {
        assert_eq!(locate_track(&two_tracks(), 1800.0), (1, 0.0));
    }

    #[test]
    fn locate_track_before_the_first_track_clamps_to_it() {
        assert_eq!(locate_track(&two_tracks(), -5.0), (0, 0.0));
    }

    #[test]
    fn locate_track_with_no_tracks_does_not_panic() {
        assert_eq!(locate_track(&[], 120.0), (0, 0.0));
    }

    #[tokio::test]
    async fn sync_progress_to_server_propagates_server_errors() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let result = sync_progress_to_server(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1", 42.5, 100.0, false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sync_progress_to_server_maps_401_to_auth() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(401)).mount(&mock_server).await;

        let result = sync_progress_to_server(&ConnectionTarget::direct(&mock_server.uri()), "test-token", "item-1", 42.5, 100.0, false).await;

        assert!(matches!(result, Err(CoreError::Auth)), "got: {result:?}");
    }
}
