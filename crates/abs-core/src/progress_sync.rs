//! Reconciles locally recorded playback progress against the server's, so progress made on
//! another device (or one of the official apps) shows up here too — not just this client's own
//! writes (`abs_core::streaming::sync_progress_to_server` handles the other direction: pushing a
//! local write up to the server).
//!
//! Every call here is best-effort and bounded: it uses a short timeout (much shorter than the
//! 15s default used for calls that are actually required to proceed, like resolving a stream
//! URL), and callers are expected to treat a failure — offline, a slow connection, a server
//! that's simply gone — as "nothing to reconcile, fall back to local storage" rather than letting
//! it block anything. Reading progress off the network is a nice-to-have, never a requirement.

use std::time::Duration;

// The stringify-into-`CoreError` boundaries below use `error_chain` rather than plain
// `to_string()`: reqwest's Display is just "error sending request for url (...)" for every
// transport failure — timeout, DNS and TLS certificate errors are indistinguishable without
// the `source()` chain it never prints.
use abs_api::error_chain;

use crate::error::{CoreError, Result};

/// Best-effort work should never be felt as a hang — this is deliberately much shorter than the
/// 15s default `abs_api::Client::with_bearer_token` uses for calls actually required to proceed
/// (e.g. resolving a playable URL).
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(5);

/// `Unreachable` when the server never answered, otherwise `UnexpectedResponse` with the full
/// error chain.
fn progress_error(err: &abs_api::LibraryItemsError) -> CoreError {
    if err.is_unreachable() {
        CoreError::Unreachable(error_chain(err))
    } else {
        CoreError::UnexpectedResponse(error_chain(err))
    }
}

/// Pulls a single item's progress from the server and, if the server's copy is newer than (or
/// there is no) local record, overwrites local storage with it. Called right before starting
/// playback, so resuming reflects the freshest progress across devices rather than only this
/// client's own last local write.
pub async fn reconcile_item_progress(
    pool: &sqlx::SqlitePool,
    connection: &crate::connection::ConnectionTarget,
    access_token: &str,
    account_id: &str,
    server_id: &str,
    item_id: &str,
) -> Result<()> {
    let api = connection
        .api_client_with_timeout(access_token, RECONCILE_TIMEOUT)
        .map_err(|e| CoreError::UnexpectedResponse(error_chain(&e)))?;
    let Some(server_progress) =
        api.get_media_progress(item_id).await.map_err(|e| progress_error(&e))?
    else {
        return Ok(());
    };

    apply_if_newer(pool, account_id, server_id, item_id, &server_progress).await
}

/// Writes an item's progress directly — local storage first, then a best-effort push to the
/// server (a failure here is the caller's to log; never fatal, same posture
/// `sync_progress_to_server` itself documents) — for a caller that isn't necessarily driving live
/// playback for this item (e.g. Item Detail's "Mark as finished"/"Reset progress" acting on a
/// book that isn't the one currently loaded into the player). The player's own write path
/// (`app::player::PlayerController`) has the identical two steps for whatever item it currently
/// has loaded; this is the same shape for an arbitrary item id.
#[allow(clippy::too_many_arguments)]
pub async fn push_item_progress(
    pool: &sqlx::SqlitePool,
    connection: &crate::connection::ConnectionTarget,
    access_token: &str,
    account_id: &str,
    server_id: &str,
    item_id: &str,
    position_seconds: f64,
    duration_seconds: f64,
    is_finished: bool,
) -> Result<()> {
    abs_storage::repo::progress::set(pool, account_id, server_id, item_id, position_seconds, is_finished).await?;
    crate::streaming::sync_progress_to_server(connection, access_token, item_id, position_seconds, duration_seconds, is_finished).await?;
    abs_storage::repo::progress::mark_pushed(pool, account_id, server_id, item_id, position_seconds, is_finished).await?;
    Ok(())
}

/// Bulk version of [`reconcile_item_progress`] for Home's "Continue Listening" shelf — one
/// `/api/me` call instead of one per item. Progress for an item this client hasn't synced into
/// its local `items` table yet (e.g. a library it hasn't opened) is skipped rather than erroring:
/// the local `progress` row has a foreign key on `items`, and there is nothing useful to show for
/// an item Home doesn't otherwise know about.
///
/// Runs in both directions: local progress the server hasn't confirmed yet (listened to offline,
/// or whose push failed) is pushed first, unless the server has something newer for that item.
/// Before, such progress was only ever retried while the same book stayed loaded in the player.
///
/// `skip_item` is the book the player has loaded, if any: the player owns that item's row (it
/// writes and pushes it on its own schedule, and reconciles it against the server when resuming),
/// so a push of the row's up-to-5-seconds-stale copy or an overwrite from a snapshot fetched
/// before the player's own push landed would only race it.
pub async fn reconcile_all_progress(
    pool: &sqlx::SqlitePool,
    connection: &crate::connection::ConnectionTarget,
    access_token: &str,
    account_id: &str,
    server_id: &str,
    skip_item: Option<&str>,
) -> Result<()> {
    let api = connection
        .api_client_with_timeout(access_token, RECONCILE_TIMEOUT)
        .map_err(|e| CoreError::UnexpectedResponse(error_chain(&e)))?;
    let all_progress = api.get_all_media_progress().await.map_err(|e| progress_error(&e))?;

    let pushed = push_unconfirmed_progress(pool, connection, access_token, account_id, server_id, &all_progress, skip_item).await?;

    for server_progress in &all_progress {
        // Just pushed: the server's copy fetched above is already out of date.
        if pushed.contains(&server_progress.library_item_id) || skip_item == Some(server_progress.library_item_id.as_str()) {
            continue;
        }
        if abs_storage::repo::items::get(pool, server_id, &server_progress.library_item_id).await.is_err() {
            continue;
        }
        apply_if_newer(pool, account_id, server_id, &server_progress.library_item_id, server_progress).await?;
    }
    Ok(())
}

/// Pushes every local progress row of this account on this server that the server hasn't
/// confirmed, unless the server's copy (`server_progress`, just fetched) is newer. Returns the
/// item ids it pushed. Stops at the first failed push: the server is then most likely unreachable,
/// and every row stays marked for the next attempt.
async fn push_unconfirmed_progress(
    pool: &sqlx::SqlitePool,
    connection: &crate::connection::ConnectionTarget,
    access_token: &str,
    account_id: &str,
    server_id: &str,
    server_progress: &[abs_api::ServerProgress],
    skip_item: Option<&str>,
) -> Result<std::collections::HashSet<String>> {
    let mut pushed = std::collections::HashSet::new();
    for local in abs_storage::repo::progress::list_needing_push(pool, account_id).await? {
        if local.server_id != server_id || skip_item == Some(local.item_id.as_str()) {
            continue;
        }
        let server_copy = server_progress.iter().find(|p| p.library_item_id == local.item_id);
        if let Some(server_copy) = server_copy {
            let server_updated_at = chrono::DateTime::from_timestamp_millis(server_copy.last_update_ms).unwrap_or_default();
            if server_updated_at > local.updated_at {
                tracing::info!(item_id = %local.item_id, "the server has newer progress than this device's unpushed write; keeping the server's");
                continue;
            }
        }
        let duration_seconds = match server_copy {
            Some(server_copy) if server_copy.duration_seconds > 0.0 => server_copy.duration_seconds,
            _ => abs_storage::repo::items::get(pool, server_id, &local.item_id).await.map(|item| item.duration_seconds).unwrap_or(0.0),
        };
        let push = crate::streaming::sync_progress_to_server(
            connection,
            access_token,
            &local.item_id,
            local.current_time_seconds,
            duration_seconds,
            local.is_finished,
        )
        .await;
        match push {
            Ok(()) => {}
            // The book is gone from the server (a downloaded copy can outlive it): its progress
            // has nowhere to go. Retrying it would block every later push behind it for good.
            Err(CoreError::NotOnServer(_)) => {
                tracing::info!(item_id = %local.item_id, "this book isn't on the server any more; keeping its progress on this device only");
            }
            Err(err) => return Err(err),
        }
        abs_storage::repo::progress::mark_pushed(pool, account_id, server_id, &local.item_id, local.current_time_seconds, local.is_finished)
            .await?;
        pushed.insert(local.item_id);
    }
    Ok(pushed)
}

async fn apply_if_newer(
    pool: &sqlx::SqlitePool,
    account_id: &str,
    server_id: &str,
    item_id: &str,
    server_progress: &abs_api::ServerProgress,
) -> Result<()> {
    let local = abs_storage::repo::progress::get(pool, account_id, server_id, item_id).await?;
    let server_updated_at = chrono::DateTime::from_timestamp_millis(server_progress.last_update_ms).unwrap_or_default();

    // A row the server already confirmed holds nothing the server doesn't know, so the server's
    // copy wins outright — without trusting two different clocks to order them. Only a local
    // write the server hasn't seen yet is weighed by time.
    let should_overwrite = match &local {
        None => true,
        Some(local) if !local.needs_push => true,
        Some(local) => server_updated_at > local.updated_at,
    };
    if !should_overwrite {
        return Ok(());
    }

    // The server's own last-update time is preserved, not "now" — Home's "Continue Listening"
    // shelf orders by `updated_at`, and an imported record stamped with the import time would
    // make an item last listened to years ago look more recent than one listened to today.
    abs_storage::repo::progress::set_at(
        pool,
        account_id,
        server_id,
        item_id,
        server_progress.current_time_seconds,
        server_progress.is_finished,
        server_updated_at,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ConnectionTarget;
    use abs_storage::repo::{accounts, items, libraries, servers};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn pool_with_synced_item(server_url: &str, item_id: &str) -> (sqlx::SqlitePool, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);

        let server_id = servers::add(&pool, server_url).await.unwrap();
        let account_id = accounts::add(&pool, &server_id, "jane", "token123", None).await.unwrap();
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
                title: "Test Item",
                author: None,
                narrator: None,
                description: None,
                duration_seconds: 0.0,
                added_at: chrono::Utc::now(),
                series_name: None,
                genres: &[],
            },
        )
        .await
        .unwrap();

        (pool, server_id, account_id)
    }

    #[tokio::test]
    async fn reconcile_item_progress_pulls_a_fresh_server_record_when_nothing_local_exists() {
        // Deliberately in the past: the imported row must carry the server's last-update time
        // (what "Continue Listening" orders by), not the moment of the import itself. Millis
        // precision, matching what `lastUpdate` can express over the wire.
        let last_update = chrono::DateTime::from_timestamp_millis(chrono::Utc::now().timestamp_millis() - 30 * 86_400_000).unwrap();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraryItemId": "item-1",
                "currentTime": 55.0,
                "duration": 100.0,
                "isFinished": false,
                "lastUpdate": last_update.timestamp_millis(),
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        reconcile_item_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, "item-1").await.unwrap();

        let progress = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap();
        let progress = progress.unwrap();
        assert_eq!(progress.current_time_seconds, 55.0);
        assert_eq!(progress.updated_at, last_update, "the server's last-update time must be kept, not the import time");
    }

    #[tokio::test]
    async fn reconcile_item_progress_with_no_server_record_leaves_local_storage_untouched() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(404)).mount(&mock_server).await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 12.0, false).await.unwrap();

        reconcile_item_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, "item-1").await.unwrap();

        let progress = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap();
        assert_eq!(progress.unwrap().current_time_seconds, 12.0, "no server record means nothing to reconcile");
    }

    #[tokio::test]
    async fn reconcile_item_progress_does_not_overwrite_a_newer_local_write() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraryItemId": "item-1",
                "currentTime": 5.0,
                "duration": 100.0,
                "isFinished": false,
                // Far in the past — any local write from "now" should win over this.
                "lastUpdate": 0,
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 90.0, false).await.unwrap();

        reconcile_item_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, "item-1").await.unwrap();

        let progress = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap();
        assert_eq!(progress.unwrap().current_time_seconds, 90.0, "a stale server record must not clobber a newer local write");
    }

    #[tokio::test]
    async fn reconcile_item_progress_overwrites_a_stale_local_write() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraryItemId": "item-1",
                "currentTime": 200.0,
                "duration": 300.0,
                "isFinished": false,
                "lastUpdate": chrono::Utc::now().timestamp_millis() + 60_000,
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 10.0, false).await.unwrap();

        reconcile_item_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, "item-1").await.unwrap();

        let progress = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap();
        assert_eq!(progress.unwrap().current_time_seconds, 200.0, "a newer server record should win over a stale local write");
    }

    #[tokio::test]
    async fn reconcile_item_progress_fails_fast_when_the_server_is_unreachable() {
        // No mock server at all — a closed port refuses immediately rather than hanging, so this
        // also verifies the call doesn't silently succeed against nothing.
        let result = reconcile_item_progress(&sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap(), &ConnectionTarget::direct("http://127.0.0.1:1"), "token", "acc-1", "srv-1", "item-1").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reconcile_all_progress_skips_items_not_synced_locally() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "mediaProgress": [
                    { "libraryItemId": "item-unknown", "currentTime": 1.0, "duration": 10.0, "isFinished": false, "lastUpdate": 1 },
                    { "libraryItemId": "item-1", "currentTime": 42.0, "duration": 100.0, "isFinished": false, "lastUpdate": chrono::Utc::now().timestamp_millis() },
                ]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        reconcile_all_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, None).await.unwrap();

        let known = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap();
        assert_eq!(known.unwrap().current_time_seconds, 42.0);
        let unknown = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-unknown").await.unwrap();
        assert!(unknown.is_none(), "progress for an item Home hasn't synced yet should be skipped, not error");
    }

    #[tokio::test]
    async fn reconcile_all_progress_pushes_unconfirmed_local_progress_first() {
        let mock_server = MockServer::start().await;
        let two_days_ago = chrono::Utc::now().timestamp_millis() - 2 * 86_400_000;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "mediaProgress": [
                    { "libraryItemId": "item-1", "currentTime": 10.0, "duration": 100.0, "isFinished": false, "lastUpdate": two_days_ago },
                ]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server).await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        // Listened offline: newer than the server's copy, never pushed.
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 70.0, false).await.unwrap();

        reconcile_all_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, None).await.unwrap();

        let requests = mock_server.received_requests().await.unwrap();
        let patch = requests.iter().find(|r| r.method.as_str() == "PATCH").expect("the unconfirmed local progress should be pushed");
        let body: serde_json::Value = serde_json::from_slice(&patch.body).unwrap();
        assert_eq!(body["currentTime"], 70.0);
        let local = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap().unwrap();
        assert_eq!(local.current_time_seconds, 70.0, "the server's stale copy must not overwrite what was just pushed");
        assert!(!local.needs_push, "a successful push clears the flag");
    }

    /// A downloaded book can outlive its removal from the server. Its unpushed progress gets a
    /// 404, which used to stop every later push behind it for good.
    #[tokio::test]
    async fn a_book_gone_from_the_server_does_not_block_other_pushes() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/me")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "mediaProgress": [] }))).mount(&mock_server).await;
        Mock::given(method("PATCH")).and(path("/api/me/progress/gone")).respond_with(ResponseTemplate::new(404)).mount(&mock_server).await;
        Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&mock_server).await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        let gone = items::UpsertItem { id: "gone", server_id: &server_id, library_id: "lib-1", title: "Gone", author: None, narrator: None, description: None, duration_seconds: 0.0, added_at: chrono::Utc::now(), series_name: None, genres: &[] };
        items::upsert(&pool, gone).await.unwrap();
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "gone", 30.0, false).await.unwrap();
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 70.0, false).await.unwrap();

        reconcile_all_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, None).await.unwrap();

        for item_id in ["gone", "item-1"] {
            let local = abs_storage::repo::progress::get(&pool, &account_id, &server_id, item_id).await.unwrap().unwrap();
            assert!(!local.needs_push, "{item_id}: nothing left to push");
        }
        let gone_local = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "gone").await.unwrap().unwrap();
        assert_eq!(gone_local.current_time_seconds, 30.0, "the position stays on this device");
    }

    #[tokio::test]
    async fn reconcile_all_progress_keeps_newer_server_progress_over_an_unconfirmed_local_write() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "mediaProgress": [
                    { "libraryItemId": "item-1", "currentTime": 90.0, "duration": 100.0, "isFinished": false, "lastUpdate": chrono::Utc::now().timestamp_millis() + 60_000 },
                ]
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 70.0, false).await.unwrap();

        reconcile_all_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, None).await.unwrap();

        let requests = mock_server.received_requests().await.unwrap();
        assert!(!requests.iter().any(|r| r.method.as_str() == "PATCH"), "an older local write must not be pushed over newer server progress");
        let local = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap().unwrap();
        assert_eq!(local.current_time_seconds, 90.0);
    }

    #[tokio::test]
    async fn reconcile_item_progress_lets_the_server_win_over_a_confirmed_row_regardless_of_clocks() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraryItemId": "item-1",
                "currentTime": 300.0,
                "duration": 400.0,
                "isFinished": false,
                // Older than the local row by this device's clock — as if this device's clock ran
                // ahead of the server's.
                "lastUpdate": chrono::Utc::now().timestamp_millis() - 3_600_000,
            })))
            .mount(&mock_server)
            .await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 100.0, false).await.unwrap();
        abs_storage::repo::progress::mark_pushed(&pool, &account_id, &server_id, "item-1", 100.0, false).await.unwrap();

        reconcile_item_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, "item-1").await.unwrap();

        let local = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap().unwrap();
        assert_eq!(local.current_time_seconds, 300.0, "a row the server already confirmed must take the server's newer value");
    }

    #[tokio::test]
    async fn push_item_progress_writes_locally_and_pushes_to_the_server() {
        let mock_server = MockServer::start().await;
        Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server).await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        push_item_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, "item-1", 400.0, 400.0, true)
            .await
            .unwrap();

        let progress = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap().unwrap();
        assert_eq!(progress.current_time_seconds, 400.0);
        assert!(progress.is_finished);

        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.iter().filter(|r| r.method.as_str() == "PATCH").count(), 1, "the local write should also be pushed to the server");
    }

    #[tokio::test]
    async fn push_item_progress_still_writes_locally_when_the_server_is_unreachable() {
        let (pool, server_id, account_id) = pool_with_synced_item("http://127.0.0.1:1", "item-1").await;
        let result = push_item_progress(&pool, &ConnectionTarget::direct("http://127.0.0.1:1"), "token", &account_id, &server_id, "item-1", 0.0, 400.0, false).await;
        assert!(result.is_err(), "a failed server push should still surface as an error to the caller");

        let progress = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap().unwrap();
        assert_eq!(progress.current_time_seconds, 0.0, "the local write must land even though the server push failed");
    }

    #[tokio::test]
    async fn reconcile_all_progress_leaves_the_skipped_item_to_its_owner() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "mediaProgress": [
                    { "libraryItemId": "item-1", "currentTime": 10.0, "duration": 100.0, "isFinished": false, "lastUpdate": chrono::Utc::now().timestamp_millis() + 60_000 },
                ]
            })))
            .mount(&mock_server)
            .await;
        Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server).await;

        let (pool, server_id, account_id) = pool_with_synced_item(&mock_server.uri(), "item-1").await;
        abs_storage::repo::progress::set(&pool, &account_id, &server_id, "item-1", 70.0, false).await.unwrap();

        reconcile_all_progress(&pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &account_id, &server_id, Some("item-1")).await.unwrap();

        let requests = mock_server.received_requests().await.unwrap();
        assert!(!requests.iter().any(|r| r.method.as_str() == "PATCH"), "the loaded item's row must not be pushed from here");
        let local = abs_storage::repo::progress::get(&pool, &account_id, &server_id, "item-1").await.unwrap().unwrap();
        assert_eq!(local.current_time_seconds, 70.0, "nor overwritten with the server's snapshot");
        assert!(local.needs_push, "it stays marked for its owner to push");
    }
}
