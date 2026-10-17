//! Sends the listening sessions recorded on this device to the server, the way Audiobookshelf's
//! own apps do for downloaded books: `POST /api/session/local-all`, many sessions per request.
//! That's what the server's listening history and stats are built from — `PATCH
//! /api/me/progress` alone moves the position and leaves no trace there — and the server applies
//! a session's position only when nothing newer has reached it, so a stale device can't drag
//! progress back.
//!
//! Built for a phone: everything waiting goes in one request (chunks of [`MAX_BATCH`]), failures
//! back off instead of retrying on every tick ([`Backoff`]), and timestamps are shifted onto the
//! server's clock ([`ClockOffsets`]), because the server compares its own time with ours. When to
//! send is the player's business; this module only sends.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use abs_api::{DeviceInfo, LibraryItemsError, LocalSession};
use abs_storage::models::ListeningSession;

use crate::error::{CoreError, Result};

/// Sessions per request. A session is a few hundred bytes, so this keeps a request small even
/// after weeks offline, while one request still covers what normally piles up.
pub const MAX_BATCH: i64 = 50;

/// How long sent, closed sessions are kept before [`prune`] deletes them.
pub const KEEP_SENT_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The settings key holding this install's device id.
const DEVICE_ID_KEY: &str = "device.id";

/// Audiobookshelf's `PlayMethod` for a book streamed from the server.
pub const PLAY_METHOD_DIRECT_PLAY: i64 = 0;
/// Audiobookshelf's `PlayMethod` for a book played from a downloaded file.
pub const PLAY_METHOD_LOCAL: i64 = 3;

/// A new session id: a random UUID, which the server keeps as the session's own id. (A
/// `play_local_` prefix would be remapped only in the server's memory, so a server restart would
/// turn the next send of the same session into a duplicate.)
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// This device as sent with every batch. The id is generated once per install and kept in the
/// settings table: the server keys its device records on it and, without one, adds a new device
/// row for every request.
pub async fn device_info(pool: &sqlx::SqlitePool) -> Result<DeviceInfo> {
    let device_id = match abs_storage::repo::settings::get(pool, DEVICE_ID_KEY).await? {
        Some(id) if !id.trim().is_empty() => id,
        _ => {
            let id = uuid::Uuid::new_v4().to_string();
            abs_storage::repo::settings::set(pool, DEVICE_ID_KEY, &id).await?;
            id
        }
    };
    static HARDWARE: OnceLock<(Option<String>, Option<String>)> = OnceLock::new();
    let (manufacturer, model) = HARDWARE.get_or_init(|| hardware_name(Path::new("/"))).clone();
    Ok(DeviceInfo {
        device_id,
        client_name: "Audiobooklet".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        manufacturer,
        model,
    })
}

/// The device's maker and model, for the server's device list: the device tree's `model` on a
/// phone ("Purism Librem 5r4" → "Purism", "Librem 5r4"), DMI's vendor and product on a PC. `root`
/// is `/` outside tests.
fn hardware_name(root: &Path) -> (Option<String>, Option<String>) {
    let read = |path: &str| {
        std::fs::read_to_string(root.join(path))
            .ok()
            .map(|text| text.trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string())
            .filter(|text| !text.is_empty() && !is_placeholder(text))
    };
    if let Some(model) = read("sys/firmware/devicetree/base/model") {
        return match model.split_once(' ') {
            Some((maker, rest)) if !rest.trim().is_empty() => (Some(maker.to_string()), Some(rest.trim().to_string())),
            _ => (None, Some(model)),
        };
    }
    (read("sys/class/dmi/id/sys_vendor"), read("sys/class/dmi/id/product_name"))
}

/// Firmware filler that names no real hardware.
fn is_placeholder(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    ["to be filled", "default string", "system product name", "system manufacturer", "not specified", "o.e.m."]
        .iter()
        .any(|filler| text.contains(filler))
}

/// How far each server's clock is from this device's, learned from the `Date` header of its
/// replies. The server decides whether a session's position is newer than the progress it has by
/// comparing the session's `updatedAt` (our clock) with its own; a phone clock that's behind
/// would make fresh listening look stale. Kept in memory: a restart learns it again from the
/// first reply.
#[derive(Default)]
pub struct ClockOffsets {
    samples: HashMap<String, VecDeque<i64>>,
}

impl ClockOffsets {
    const SAMPLES: usize = 5;

    /// Records a reply's `Date` (whole seconds, so half a second is added to center it) against
    /// the local time it arrived.
    pub fn record(&mut self, server_id: &str, server_date: chrono::DateTime<chrono::Utc>, local_now_ms: i64) {
        let samples = self.samples.entry(server_id.to_string()).or_default();
        samples.push_back(server_date.timestamp_millis() + 500 - local_now_ms);
        while samples.len() > Self::SAMPLES {
            samples.pop_front();
        }
    }

    /// The median of the recent samples, in milliseconds to add to local time to get the
    /// server's. `None` until a reply has been seen.
    pub fn offset_ms(&self, server_id: &str) -> Option<i64> {
        let samples = self.samples.get(server_id).filter(|samples| !samples.is_empty())?;
        let mut sorted: Vec<i64> = samples.iter().copied().collect();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }
}

/// The offsets of every server this process has heard from.
pub fn clock_offsets() -> &'static Mutex<ClockOffsets> {
    static OFFSETS: OnceLock<Mutex<ClockOffsets>> = OnceLock::new();
    OFFSETS.get_or_init(Mutex::default)
}

/// Offsets smaller than this are left alone: the `Date` header only has whole seconds.
const NOTICEABLE_OFFSET_MS: i64 = 1_500;

/// Waits between failed sends — server down or unreachable while the phone is online — instead of
/// trying again on every tick: 30 s, then doubling up to 15 min. Reset by a success, the network
/// coming back or offline mode being turned off.
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    failures: u32,
    until: Option<std::time::Instant>,
}

impl Backoff {
    const FIRST: Duration = Duration::from_secs(30);
    const MAX: Duration = Duration::from_secs(15 * 60);

    /// The wait after the `failures`-th failure in a row.
    pub fn delay(failures: u32) -> Duration {
        if failures == 0 {
            return Duration::ZERO;
        }
        Self::FIRST.saturating_mul(2u32.saturating_pow(failures - 1)).min(Self::MAX)
    }

    pub fn failed(&mut self, now: std::time::Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let delay = Self::delay(self.failures);
        self.until = Some(now + delay);
        delay
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Whether a send may be tried at `now`.
    pub fn allows(&self, now: std::time::Instant) -> bool {
        self.until.is_none_or(|until| now >= until)
    }
}

/// What one [`send_pending`] call did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SendReport {
    /// Sessions the server stored.
    pub sent: usize,
    /// Of those, how many moved the book's progress on the server.
    pub progress_updated: usize,
    /// Of those, how many the server didn't apply because it had newer progress.
    pub server_newer: usize,
    /// Sessions dropped because the server doesn't have their book.
    pub dropped: usize,
    /// Sessions the server refused for another reason; they're kept and tried again later.
    pub refused: usize,
    /// The server doesn't take local sessions, so progress went through `PATCH
    /// /api/me/progress` instead.
    pub fell_back: bool,
}

impl SendReport {
    pub fn is_empty(&self) -> bool {
        self.sent == 0 && self.dropped == 0 && self.refused == 0 && !self.fell_back
    }
}

fn session_error(err: LibraryItemsError) -> CoreError {
    match err {
        LibraryItemsError::Unauthorized(_) => CoreError::Auth,
        err if err.is_unreachable() => CoreError::Unreachable(abs_api::error_chain(&err)),
        err => CoreError::UnexpectedResponse(abs_api::error_chain(&err)),
    }
}

/// Sends every session of `account_id` on `server_id` that the server hasn't confirmed, in as few
/// requests as [`MAX_BATCH`] allows, and records what the server did with each. A transport or
/// server error stops it and leaves everything unsent for the next try.
pub async fn send_pending(
    pool: &sqlx::SqlitePool,
    api: &abs_api::Client,
    device: &DeviceInfo,
    account_id: &str,
    server_id: &str,
) -> Result<SendReport> {
    let mut report = SendReport::default();
    loop {
        let rows = abs_storage::repo::listening_sessions::list_dirty(pool, account_id, server_id, MAX_BATCH).await?;
        if rows.is_empty() {
            break;
        }
        let progress_before = report.sent + report.dropped;
        if !send_chunk(pool, api, device, account_id, server_id, &rows, &mut report).await? {
            break;
        }
        // Everything refused: trying the same rows again now would only loop.
        if report.sent + report.dropped == progress_before || rows.len() < MAX_BATCH as usize {
            break;
        }
    }
    if !report.is_empty() {
        tracing::info!(
            sent = report.sent,
            progress_updated = report.progress_updated,
            server_newer = report.server_newer,
            dropped = report.dropped,
            refused = report.refused,
            fell_back = report.fell_back,
            "sent listening sessions to the server"
        );
    }
    Ok(report)
}

/// Sends one chunk. Returns `false` when sending should stop (the fallback already handled
/// everything).
async fn send_chunk(
    pool: &sqlx::SqlitePool,
    api: &abs_api::Client,
    device: &DeviceInfo,
    account_id: &str,
    server_id: &str,
    rows: &[ListeningSession],
    report: &mut SendReport,
) -> Result<bool> {
    let offset_before = clock_offsets().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).offset_ms(server_id);
    let payload = to_payload(pool, rows, offset_before.unwrap_or(0)).await?;
    let sync = match api.sync_local_sessions(device, &payload).await {
        Ok(sync) => sync,
        Err(LibraryItemsError::NotFound(_)) => {
            fall_back_to_progress(pool, api, account_id, server_id, rows).await?;
            report.fell_back = true;
            return Ok(false);
        }
        Err(err) => return Err(session_error(err)),
    };
    let offset_after = record_server_date(server_id, sync.server_date);

    let mut results = sync.results;
    // The first batch after a start goes out before the server's clock is known. If that clock
    // turns out to be well ahead of ours, the server may have judged fresh listening stale: send
    // those sessions once more, stamped on its clock.
    if offset_before.is_none() {
        if let Some(offset) = offset_after.filter(|offset| *offset >= NOTICEABLE_OFFSET_MS) {
            let stale: Vec<ListeningSession> = rows
                .iter()
                .filter(|row| results.iter().any(|result| result.id == row.id && result.success && !result.progress_synced))
                .cloned()
                .collect();
            if !stale.is_empty() {
                tracing::info!(offset_ms = offset, sessions = stale.len(), "the server's clock is ahead of this device's; sending again on its clock");
                let payload = to_payload(pool, &stale, offset).await?;
                if let Ok(again) = api.sync_local_sessions(device, &payload).await {
                    for result in again.results {
                        if let Some(existing) = results.iter_mut().find(|existing| existing.id == result.id) {
                            *existing = result;
                        }
                    }
                }
            }
        }
    }

    for row in rows {
        let Some(result) = results.iter().find(|result| result.id == row.id) else {
            report.refused += 1;
            continue;
        };
        if result.success {
            abs_storage::repo::listening_sessions::mark_sent(pool, &row.id, row.updated_at).await?;
            report.sent += 1;
            // Either way the server's progress is settled for this position: applied, or kept
            // because something newer reached it. A PATCH of it later would undo the server's
            // decision.
            abs_storage::repo::progress::mark_pushed(pool, account_id, server_id, &row.item_id, row.current_position, false).await?;
            if result.progress_synced {
                report.progress_updated += 1;
            } else {
                report.server_newer += 1;
                tracing::info!(item_id = %row.item_id, position = row.current_position, "the server has newer progress for this book than this session; it kept its own");
            }
        } else if result.is_item_missing() {
            tracing::info!(item_id = %row.item_id, "this book isn't on the server any more; dropping its listening session");
            abs_storage::repo::listening_sessions::delete(pool, &row.id).await?;
            report.dropped += 1;
        } else {
            tracing::warn!(item_id = %row.item_id, error = result.error.as_deref().unwrap_or(""), "the server refused a listening session; will try again later");
            report.refused += 1;
        }
    }
    Ok(true)
}

/// Records a reply's `Date`, returning the offset as now known.
fn record_server_date(server_id: &str, server_date: Option<chrono::DateTime<chrono::Utc>>) -> Option<i64> {
    let mut offsets = clock_offsets().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(date) = server_date {
        offsets.record(server_id, date, chrono::Utc::now().timestamp_millis());
    }
    offsets.offset_ms(server_id)
}

async fn to_payload(pool: &sqlx::SqlitePool, rows: &[ListeningSession], offset_ms: i64) -> Result<Vec<LocalSession>> {
    let mut payload = Vec::with_capacity(rows.len());
    for row in rows {
        // The item is there: sessions are deleted with their book (foreign key).
        let item = abs_storage::repo::items::get(pool, &row.server_id, &row.item_id).await.ok();
        payload.push(LocalSession {
            id: row.id.clone(),
            library_item_id: row.item_id.clone(),
            library_id: item.as_ref().map(|item| item.library_id.clone()),
            media_type: "book",
            play_method: row.play_method,
            media_player: "gstreamer",
            display_title: item.as_ref().map(|item| item.title.clone()).unwrap_or_default(),
            display_author: item.as_ref().and_then(|item| item.author.clone()),
            duration: row.duration,
            start_time: row.start_position,
            current_time: row.current_position,
            time_listening: row.time_listening,
            started_at: row.started_at + offset_ms,
            updated_at: row.updated_at + offset_ms,
        });
    }
    Ok(payload)
}

/// For a server without local sessions (too old, or a proxy in the way): the newest position of
/// each book goes through `PATCH /api/me/progress`, as before sessions. The listening itself
/// can't be recorded there, so the sessions are marked sent rather than kept forever.
async fn fall_back_to_progress(
    pool: &sqlx::SqlitePool,
    api: &abs_api::Client,
    account_id: &str,
    server_id: &str,
    rows: &[ListeningSession],
) -> Result<()> {
    tracing::info!(server_id, "this server doesn't take listening sessions; sending progress only");
    let mut newest: HashMap<&str, &ListeningSession> = HashMap::new();
    for row in rows {
        let entry = newest.entry(row.item_id.as_str()).or_insert(row);
        if row.updated_at > entry.updated_at {
            *entry = row;
        }
    }
    for row in newest.values() {
        match api.update_media_progress(&row.item_id, row.current_position, row.duration, false).await {
            Ok(()) => abs_storage::repo::progress::mark_pushed(pool, account_id, server_id, &row.item_id, row.current_position, false).await?,
            Err(LibraryItemsError::NotFound(_)) => {}
            Err(err) => return Err(session_error(err)),
        }
    }
    for row in rows {
        abs_storage::repo::listening_sessions::mark_sent(pool, &row.id, row.updated_at).await?;
    }
    Ok(())
}

/// Housekeeping at startup: sessions a crash left open are closed (a new play starts a new one),
/// and sessions the server has had in full for [`KEEP_SENT_FOR`] are deleted.
pub async fn tidy_up(pool: &sqlx::SqlitePool) -> Result<()> {
    abs_storage::repo::listening_sessions::close_all_open(pool).await?;
    let cutoff = chrono::Utc::now().timestamp_millis() - KEEP_SENT_FOR.as_millis() as i64;
    abs_storage::repo::listening_sessions::prune_sent(pool, cutoff).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use abs_storage::repo::{accounts, items, libraries, listening_sessions, progress, servers};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn pool_with_books(server_url: &str, books: usize) -> (sqlx::SqlitePool, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = servers::add(&pool, server_url).await.unwrap();
        let account_id = accounts::add(&pool, &server_id, "jane", "tok", None).await.unwrap();
        libraries::upsert(
            &pool,
            libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        for n in 1..=books {
            items::upsert(
                &pool,
                items::UpsertItem {
                    id: &format!("item-{n}"),
                    server_id: &server_id,
                    library_id: "lib-1",
                    title: &format!("Book {n}"),
                    author: Some("Author"),
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
        }
        (pool, server_id, account_id)
    }

    fn row(id: &str, account_id: &str, server_id: &str, item_id: &str, updated_at: i64) -> ListeningSession {
        ListeningSession {
            id: id.to_string(),
            account_id: account_id.to_string(),
            server_id: server_id.to_string(),
            item_id: item_id.to_string(),
            play_method: PLAY_METHOD_LOCAL,
            start_position: 100.0,
            current_position: 160.0,
            time_listening: 60.0,
            duration: 3600.0,
            started_at: updated_at - 60_000,
            updated_at,
            sent_updated_at: None,
            closed: false,
        }
    }

    fn device() -> DeviceInfo {
        DeviceInfo { device_id: "dev".into(), client_name: "Audiobooklet".into(), client_version: "1".into(), manufacturer: None, model: None }
    }

    /// Answers every session of a `local-all` request with `result(id)`.
    struct Answer(fn(&str) -> serde_json::Value);

    impl wiremock::Respond for Answer {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let body: serde_json::Value = request.body_json().unwrap();
            let results: Vec<_> = body["sessions"].as_array().unwrap().iter().map(|s| (self.0)(s["id"].as_str().unwrap())).collect();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": results }))
        }
    }

    fn ok(id: &str) -> serde_json::Value {
        serde_json::json!({ "id": id, "success": true, "progressSynced": true })
    }

    #[tokio::test]
    async fn every_book_s_sessions_go_in_one_request_and_are_marked_sent() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/session/local-all")).respond_with(Answer(ok)).expect(1).mount(&server).await;
        let (pool, server_id, account_id) = pool_with_books(&server.uri(), 3).await;
        for n in 1..=3 {
            listening_sessions::save(&pool, &row(&format!("s-{n}"), &account_id, &server_id, &format!("item-{n}"), 1_000 * n)).await.unwrap();
            progress::set(&pool, &account_id, &server_id, &format!("item-{n}"), 160.0, false).await.unwrap();
        }

        let report = send_pending(&pool, &abs_api::Client::new(&server.uri()), &device(), &account_id, &server_id).await.unwrap();
        assert_eq!(report, SendReport { sent: 3, progress_updated: 3, ..Default::default() });
        assert_eq!(listening_sessions::count_dirty(&pool, &account_id, &server_id).await.unwrap(), 0);
        assert!(progress::list_needing_push(&pool, &account_id).await.unwrap().is_empty(), "the session carried the position");

        let body: serde_json::Value = server.received_requests().await.unwrap()[0].body_json().unwrap();
        let first = &body["sessions"][0];
        assert_eq!(first["displayTitle"], "Book 1");
        assert_eq!(first["displayAuthor"], "Author");
        assert_eq!(first["libraryId"], "lib-1");
        assert_eq!(first["playMethod"], 3);
        assert_eq!(first["timeListening"], 60.0);

        // Nothing dirty: nothing sent.
        let again = send_pending(&pool, &abs_api::Client::new(&server.uri()), &device(), &account_id, &server_id).await.unwrap();
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn a_long_backlog_goes_in_chunks_of_max_batch() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/session/local-all")).respond_with(Answer(ok)).expect(2).mount(&server).await;
        let (pool, server_id, account_id) = pool_with_books(&server.uri(), 1).await;
        for n in 0..(MAX_BATCH + 10) {
            listening_sessions::save(&pool, &row(&format!("s-{n}"), &account_id, &server_id, "item-1", 1_000 + n)).await.unwrap();
        }
        let report = send_pending(&pool, &abs_api::Client::new(&server.uri()), &device(), &account_id, &server_id).await.unwrap();
        assert_eq!(report.sent, (MAX_BATCH + 10) as usize);
        let sizes: Vec<usize> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.body_json::<serde_json::Value>().unwrap()["sessions"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, [MAX_BATCH as usize, 10]);
    }

    #[tokio::test]
    async fn each_kind_of_result_is_handled_and_refused_sessions_stay_for_later() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/session/local-all"))
            .respond_with(Answer(|id| match id {
                "applied" => serde_json::json!({ "id": id, "success": true, "progressSynced": true }),
                "kept" => serde_json::json!({ "id": id, "success": true, "progressSynced": false }),
                "gone" => serde_json::json!({ "id": id, "success": false, "error": "Media item not found" }),
                _ => serde_json::json!({ "id": id, "success": false, "error": "Library not found" }),
            }))
            .mount(&server)
            .await;
        let (pool, server_id, account_id) = pool_with_books(&server.uri(), 4).await;
        for (n, id) in ["applied", "kept", "gone", "refused"].iter().enumerate() {
            listening_sessions::save(&pool, &row(id, &account_id, &server_id, &format!("item-{}", n + 1), 1_000)).await.unwrap();
        }
        progress::set(&pool, &account_id, &server_id, "item-2", 160.0, false).await.unwrap();

        let report = send_pending(&pool, &abs_api::Client::new(&server.uri()), &device(), &account_id, &server_id).await.unwrap();
        assert_eq!(report, SendReport { sent: 2, progress_updated: 1, server_newer: 1, dropped: 1, refused: 1, fell_back: false });
        assert!(listening_sessions::get(&pool, "gone").await.unwrap().is_none());
        let dirty: Vec<String> = listening_sessions::list_dirty(&pool, &account_id, &server_id, 50).await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(dirty, ["refused"]);
        assert!(
            !progress::get(&pool, &account_id, &server_id, "item-2").await.unwrap().unwrap().needs_push,
            "the server kept newer progress: don't PATCH over it later"
        );
    }

    #[tokio::test]
    async fn a_server_without_local_sessions_gets_the_newest_position_per_book_by_patch() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/session/local-all")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
        Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
        let (pool, server_id, account_id) = pool_with_books(&server.uri(), 1).await;
        let mut older = row("older", &account_id, &server_id, "item-1", 1_000);
        older.current_position = 50.0;
        listening_sessions::save(&pool, &older).await.unwrap();
        listening_sessions::save(&pool, &row("newer", &account_id, &server_id, "item-1", 2_000)).await.unwrap();

        let report = send_pending(&pool, &abs_api::Client::new(&server.uri()), &device(), &account_id, &server_id).await.unwrap();
        assert!(report.fell_back);
        let patch = server.received_requests().await.unwrap().into_iter().find(|r| r.method.as_str() == "PATCH").unwrap();
        assert_eq!(patch.body_json::<serde_json::Value>().unwrap()["currentTime"], 160.0, "the newest session's position");
        assert_eq!(listening_sessions::count_dirty(&pool, &account_id, &server_id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rejected_credentials_and_an_unreachable_server_leave_everything_unsent() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/api/session/local-all")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
        let (pool, server_id, account_id) = pool_with_books(&server.uri(), 1).await;
        listening_sessions::save(&pool, &row("s-1", &account_id, &server_id, "item-1", 1_000)).await.unwrap();

        let err = send_pending(&pool, &abs_api::Client::new(&server.uri()), &device(), &account_id, &server_id).await.unwrap_err();
        assert!(matches!(err, CoreError::Auth));
        let err = send_pending(&pool, &abs_api::Client::new("http://127.0.0.1:9"), &device(), &account_id, &server_id).await.unwrap_err();
        assert!(matches!(err, CoreError::Unreachable(_)), "{err:?}");
        assert_eq!(listening_sessions::count_dirty(&pool, &account_id, &server_id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_server_clock_well_ahead_gets_a_second_send_stamped_on_its_clock() {
        let server = MockServer::start().await;
        // The server's clock is 10 minutes ahead; it calls a session stale unless stamped after
        // its own "now".
        struct AheadServer;
        impl wiremock::Respond for AheadServer {
            fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
                let server_now = chrono::Utc::now() + chrono::Duration::minutes(10);
                let body: serde_json::Value = request.body_json().unwrap();
                let results: Vec<_> = body["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| {
                        let fresh = s["updatedAt"].as_i64().unwrap() >= server_now.timestamp_millis() - 60_000;
                        serde_json::json!({ "id": s["id"], "success": true, "progressSynced": fresh })
                    })
                    .collect();
                ResponseTemplate::new(200).insert_header("date", server_now.to_rfc2822().replace("+0000", "GMT")).set_body_json(serde_json::json!({ "results": results }))
            }
        }
        Mock::given(method("POST")).and(path("/api/session/local-all")).respond_with(AheadServer).mount(&server).await;
        // A server id of its own, so other tests' offsets don't leak in.
        let (pool, server_id, account_id) = pool_with_books(&format!("{}/ahead", server.uri()), 1).await;
        let now = chrono::Utc::now().timestamp_millis();
        listening_sessions::save(&pool, &row("s-1", &account_id, &server_id, "item-1", now)).await.unwrap();

        let report = send_pending(&pool, &abs_api::Client::new(&server.uri()), &device(), &account_id, &server_id).await.unwrap();
        assert_eq!(report.progress_updated, 1, "{report:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        let offset = clock_offsets().lock().unwrap().offset_ms(&server_id).unwrap();
        assert!((offset - 600_000).abs() < 3_000, "learned offset {offset}");
    }

    #[test]
    fn clock_offset_is_the_median_of_the_last_five_replies() {
        let mut offsets = ClockOffsets::default();
        assert!(offsets.offset_ms("s").is_none());
        let at = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).unwrap();
        // Server time (whole seconds) minus local arrival time, +500 ms for the truncation.
        for (server_ms, local_ms) in [(10_000, 9_000), (20_000, 19_000), (30_000, 29_000), (40_000, 1_000), (50_000, 49_000)] {
            offsets.record("s", at(server_ms), local_ms);
        }
        assert_eq!(offsets.offset_ms("s"), Some(1_500), "one wild sample doesn't move the median");
        for _ in 0..5 {
            offsets.record("s", at(10_000), 10_000);
        }
        assert_eq!(offsets.offset_ms("s"), Some(500), "only the last five count");
        assert!(offsets.offset_ms("other").is_none());
    }

    #[test]
    fn backoff_doubles_from_30_s_to_at_most_15_min_and_resets() {
        let delays: Vec<u64> = (0..=8).map(|n| Backoff::delay(n).as_secs()).collect();
        assert_eq!(delays, [0, 30, 60, 120, 240, 480, 900, 900, 900]);
        let now = std::time::Instant::now();
        let mut backoff = Backoff::default();
        assert!(backoff.allows(now));
        backoff.failed(now);
        assert!(!backoff.allows(now + Duration::from_secs(29)));
        assert!(backoff.allows(now + Duration::from_secs(30)));
        backoff.reset();
        assert!(backoff.allows(now));
        assert_eq!(backoff.failures(), 0);
    }

    #[tokio::test]
    async fn the_device_id_is_made_once_and_kept() {
        let (pool, _, _) = pool_with_books("https://a.example", 0).await;
        let first = device_info(&pool).await.unwrap();
        let second = device_info(&pool).await.unwrap();
        assert_eq!(first.device_id, second.device_id);
        assert!(uuid::Uuid::parse_str(&first.device_id).is_ok());
        assert_eq!(first.client_name, "Audiobooklet");
    }

    #[test]
    fn hardware_name_reads_the_device_tree_then_dmi_and_skips_firmware_filler() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        assert_eq!(hardware_name(root.path()), (None, None));

        write("sys/class/dmi/id/sys_vendor", "LENOVO\n");
        write("sys/class/dmi/id/product_name", "To Be Filled By O.E.M.\n");
        assert_eq!(hardware_name(root.path()), (Some("LENOVO".into()), None));

        write("sys/firmware/devicetree/base/model", "Purism Librem 5r4\0");
        assert_eq!(hardware_name(root.path()), (Some("Purism".into()), Some("Librem 5r4".into())));
    }
}
