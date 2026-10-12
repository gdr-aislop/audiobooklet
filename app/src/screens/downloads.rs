//! The Downloads tab — `docs/design/ui-spec.md`'s "Downloads" section: one row per item that has
//! been downloaded fully or partially, or is currently being fetched, with a remove/cancel action
//! and an empty state when nothing has been downloaded yet.
//!
//! Completed rows carry a gray subtitle — "10 chapters, 64.1 MB" — computed from the local
//! `download_tracks` rows (count of `Complete` tracks, summed `bytes_downloaded`). In-flight rows
//! show live progress instead: "3/10 chapters · 18.2 MB · 2.1 MB/s", driven by the manager's
//! `batch_progress` (chapters tick per track outcome) and `TrackProgress` events (bytes + a
//! smoothed speed), throttled so per-chunk events don't thrash the label — the spinner stays as
//! the at-a-glance "something is moving" cue.
//!
//! No storage-used/free-space summary row (the spec's own "nice to have") — left as a documented
//! follow-up rather than built here.
//!
//! Never imports `abs_api`: the only calls here are `abs_core::download_tracks` (read the
//! downloaded-item list) and `DownloadManager` (cancel/clear) — same boundary every other screen
//! respects.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use sqlx::SqlitePool;

use crate::downloads::{DownloadEvent, DownloadManager, ItemDownloadState};
use crate::i18n::{ntr, tr, tr_args};
use crate::screens::settings::confirm;
use abs_storage::models::DownloadStatus;

/// Minimum gap between live subtitle writes for one item. `DownloadManager` already throttles
/// `TrackProgress` itself to the same cadence (`downloads::PROGRESS_EVENT_MIN_INTERVAL`) — this
/// screen's own throttle stays regardless, since it costs nothing and keeps this file correct
/// even if that upstream cadence ever changes; the subtitle only needs ~5 updates a second either
/// way.
const LABEL_UPDATE_INTERVAL: Duration = Duration::from_millis(200);

/// A speed sample older than this means nothing has arrived in a while — hide the speed part
/// rather than showing a frozen rate as if it were current.
const SPEED_DISPLAY_WINDOW: Duration = Duration::from_secs(2);

pub struct DownloadsScreen {
    pub root: gtk4::Widget,
    #[cfg(test)]
    hooks: TestHooks,
}

#[cfg(test)]
pub struct TestHooks {
    pub status_page: adw::StatusPage,
    pub list_box: gtk4::ListBox,
    pub scroller: gtk4::ScrolledWindow,
    pub clear_all_button: gtk4::Button,
}

#[cfg(test)]
impl DownloadsScreen {
    pub fn test_hooks(&self) -> &TestHooks {
        &self.hooks
    }
}

/// Per-item speed smoothing, kept across row rebuilds (rebuilds happen on every
/// `ItemStateChanged`; the download itself doesn't reset each time).
struct SpeedState {
    ema: f64,
    last_bytes: u64,
    last_instant: Option<Instant>,
    last_label_write: Instant,
}

struct Widgets {
    pool: SqlitePool,
    server_id: String,
    list_box: gtk4::ListBox,
    status_page: adw::StatusPage,
    scroller: gtk4::ScrolledWindow,
    /// Only sensitive once there's actually something to clear — toggled alongside
    /// `status_page`/`scroller` in `spawn_refresh`, from the same `any_row`.
    clear_all_button: gtk4::Button,
    download_manager: DownloadManager,
    /// Item ids this screen has itself observed going into `Downloading` — the persisted
    /// `downloaded_item_ids` query only ever reflects *complete* tracks, so a download still in
    /// flight (nothing complete yet) needs to be tracked here to show a row for it at all.
    downloading: Rc<RefCell<HashSet<String>>>,
    /// Per-item, per-track cumulative downloaded bytes — seeded from the DB at every refresh
    /// (`bytes_downloaded` is the full size for `Complete` tracks, the last checkpoint otherwise)
    /// and updated from `TrackProgress` events (per-track cumulative, so no deltas to reconcile).
    track_bytes: Rc<RefCell<HashMap<String, HashMap<String, u64>>>>,
    /// Smoothed per-item download speed across rebuilds — see `SpeedState`.
    speeds: Rc<RefCell<HashMap<String, SpeedState>>>,
    /// The in-flight rows themselves, keyed by item id — `TrackProgress` updates a row's subtitle
    /// text in place (throttled) instead of rebuilding the whole list per chunk. Rebuilt rows
    /// re-register here at refresh.
    live_rows: Rc<RefCell<HashMap<String, adw::ActionRow>>>,
    /// Item ids whose most recent batch ended in `ItemDownloadState::Failed`, with the reason —
    /// otherwise a failed item with nothing completed simply isn't in `downloaded_item_ids` and
    /// isn't `downloading` any more either, so it would vanish from the list the instant it
    /// failed with no trace at all. Cleared the moment a new `Downloading`/`Complete`/`Stopped`
    /// state arrives for the same item (a retry, or the item leaving the list entirely).
    failed: Rc<RefCell<HashMap<String, String>>>,
    on_open: Rc<dyn Fn(crate::player::PlayRequest)>,
    /// Where a failed Remove or Clear-all reports.
    toast_overlay: adw::ToastOverlay,
}

#[allow(clippy::too_many_arguments)]
pub fn build(
    pool: SqlitePool,
    _paths: abs_storage::AppPaths,
    server: abs_storage::models::Server,
    _account: abs_storage::models::Account,
    _session: abs_core::auth::Session,
    download_manager: DownloadManager,
    window: adw::ApplicationWindow,
    // Tapping a row opens Item Detail for it — the same landing spot every other item surface
    // (Home shelf cards, Library grid) already uses, and after A1/A2/A5 it's exactly where this
    // item's own download progress and per-chapter glyphs live. `Rc<dyn Fn>`, not `impl Fn`, since
    // it's cloned into every row `spawn_refresh` rebuilds.
    on_open: Rc<dyn Fn(crate::player::PlayRequest)>,
) -> DownloadsScreen {
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&tr("Downloads"), "")));

    // Starts insensitive — nothing to clear until the first refresh below finds at least one
    // row; `spawn_refresh` toggles this the same place it already toggles
    // `status_page`/`scroller` from `any_row`. A single icon button rather than a menu: unlike
    // Player/Item Detail's `⋯` menus, there's only ever one action here.
    let clear_all_button = gtk4::Button::builder().icon_name("user-trash-symbolic").tooltip_text(tr("Clear all downloads")).css_classes(["flat"]).sensitive(false).build();
    header.pack_end(&clear_all_button);

    let status_page = adw::StatusPage::builder().icon_name("folder-download-symbolic").title(tr("No downloads yet")).vexpand(true).visible(false).build();

    let list_box = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).margin_start(12).margin_end(12).margin_top(12).build();
    let scroller = gtk4::ScrolledWindow::builder().child(&list_box).vexpand(true).build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&status_page);
    content.append(&scroller);
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&content));

    let widgets = Rc::new(Widgets {
        pool,
        server_id: server.id.clone(),
        list_box: list_box.clone(),
        status_page: status_page.clone(),
        scroller: scroller.clone(),
        clear_all_button: clear_all_button.clone(),
        download_manager: download_manager.clone(),
        // Downloads already running when this screen is built (started before a switch to another
        // server and back) show as in progress, not only once their next state change arrives.
        downloading: Rc::new(RefCell::new(download_manager.in_flight_items(&server.id).into_iter().collect())),
        track_bytes: Rc::new(RefCell::new(HashMap::new())),
        speeds: Rc::new(RefCell::new(HashMap::new())),
        live_rows: Rc::new(RefCell::new(HashMap::new())),
        failed: Rc::new(RefCell::new(HashMap::new())),
        on_open,
        toast_overlay: toast_overlay.clone(),
    });

    // Confirmed via the same shared `GtkMessageDialog` helper Settings/Connection use for their
    // own big destructive actions — unlike per-item Remove (recoverable, one book, tucked in a
    // secondary control), this wipes every downloaded file on the device at once.
    clear_all_button.connect_clicked({
        let window = window.clone();
        let download_manager = download_manager.clone();
        let server_id = widgets.server_id.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let download_manager = download_manager.clone();
            let server_id = server_id.clone();
            let toast_overlay = toast_overlay.clone();
            confirm(
                &window,
                &tr("Clear all downloads?"),
                &tr("Every downloaded chapter for every book on this device will be removed — nothing on the server is affected."),
                &tr("Clear Downloads"),
                Rc::new(move || {
                    let toast_overlay = toast_overlay.clone();
                    download_manager.clear_all(&server_id, move |result| {
                        if let Err(err) = result {
                            crate::error_reporting::report_background_error(&toast_overlay, &tr("Clearing downloads"), err);
                        }
                    })
                }),
            );
        }
    });

    // Registered once, for the manager's whole lifetime — same permanent-listener shape
    // `PlayerController::add_listener`/MPRIS already use. Every event fires on the GTK main
    // thread (the manager publishes from `spawn_future_local` futures only), so touching
    // widgets here is safe.
    download_manager.add_listener({
        let widgets = widgets.clone();
        move |event| match event {
            DownloadEvent::ItemStateChanged { item_id, state } => {
                match state {
                    ItemDownloadState::Downloading => {
                        widgets.downloading.borrow_mut().insert(item_id.clone());
                        widgets.failed.borrow_mut().remove(item_id);
                    }
                    ItemDownloadState::Failed(reason) => {
                        widgets.downloading.borrow_mut().remove(item_id);
                        // Kept (not just logged) so `spawn_refresh` can still show this item —
                        // and why it failed — instead of it silently vanishing the moment it's
                        // no longer `downloading` and has nothing `Complete` to its name.
                        widgets.failed.borrow_mut().insert(item_id.clone(), reason.clone());
                    }
                    ItemDownloadState::Idle | ItemDownloadState::Stopped | ItemDownloadState::Complete => {
                        widgets.downloading.borrow_mut().remove(item_id);
                        widgets.failed.borrow_mut().remove(item_id);
                    }
                }
                spawn_refresh(widgets.clone());
            }
            DownloadEvent::TrackProgress { item_id, ino, bytes_downloaded, .. } => {
                // Per-track cumulative bytes: just overwrite the slot — no delta math.
                widgets.track_bytes.borrow_mut().entry(item_id.clone()).or_default().insert(ino.clone(), *bytes_downloaded);

                let now = Instant::now();
                let mut speeds = widgets.speeds.borrow_mut();
                let speed = speeds.entry(item_id.clone()).or_insert(SpeedState { ema: 0.0, last_bytes: *bytes_downloaded, last_instant: None, last_label_write: now });
                match speed.last_instant {
                    // First sample of a (re)started download: seed the baseline, no rate yet.
                    None => {
                        speed.last_bytes = *bytes_downloaded;
                        speed.last_instant = Some(now);
                    }
                    Some(_) => {
                        let elapsed = now.duration_since(speed.last_instant.unwrap()).as_secs_f64();
                        if elapsed > 0.0 {
                            let instant_rate = (*bytes_downloaded).saturating_sub(speed.last_bytes) as f64 / elapsed;
                            speed.ema = if speed.ema <= 0.0 { instant_rate } else { speed.ema * 0.5 + instant_rate * 0.5 };
                            speed.last_bytes = *bytes_downloaded;
                            speed.last_instant = Some(now);
                        }
                    }
                }

                // Throttled in-place subtitle update — never a list rebuild per chunk.
                if now.duration_since(speed.last_label_write) >= LABEL_UPDATE_INTERVAL {
                    speed.last_label_write = now;
                    let ema = speed.ema;
                    drop(speeds);
                    let bytes: u64 = widgets.track_bytes.borrow().get(item_id).map(|tracks| tracks.values().sum()).unwrap_or(0);
                    let batch = widgets.download_manager.batch_progress(&widgets.server_id, item_id);
                    if let Some(row) = widgets.live_rows.borrow().get(item_id.as_str()) {
                        row.set_subtitle(&downloading_subtitle(batch, bytes, Some(ema)));
                    }
                }
            }
        }
    });

    spawn_refresh(widgets);

    DownloadsScreen {
        root: toast_overlay.upcast(),
        #[cfg(test)]
        hooks: TestHooks { status_page, list_box, scroller, clear_all_button },
    }
}

fn spawn_refresh(widgets: Rc<Widgets>) {
    adw::glib::spawn_future_local(async move {
        // A failed read must not render as "No downloads yet": the files may well be on disk.
        let (downloaded, read_failed) = match abs_core::download_tracks::downloaded_item_ids(&widgets.pool, &widgets.server_id).await {
            Ok(downloaded) => (downloaded, false),
            Err(err) => {
                tracing::warn!(%err, "couldn't read the downloads list");
                (Default::default(), true)
            }
        };
        let mut ids: Vec<String> = downloaded.into_iter().collect();
        for id in widgets.downloading.borrow().iter() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        for id in widgets.failed.borrow().keys() {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        let all_ids = ids.clone();

        // Rows are rebuilt from scratch every refresh; the in-flight rows re-register below.
        widgets.live_rows.borrow_mut().clear();

        while let Some(child) = widgets.list_box.row_at_index(0) {
            widgets.list_box.remove(&child);
        }

        let mut any_row = false;
        for item_id in ids {
            let Ok(item) = abs_storage::repo::items::get(&widgets.pool, &widgets.server_id, &item_id).await else { continue };
            let is_downloading = widgets.downloading.borrow().contains(&item_id);
            let tracks = abs_storage::repo::download_tracks::list_for_item(&widgets.pool, &widgets.server_id, &item_id).await.unwrap_or_default();

            // Seed the live byte map for this item: `bytes_downloaded` is the full size for
            // `Complete` tracks and the last checkpoint for pending/failed ones — exactly the
            // per-track cumulative value `TrackProgress` events will keep overwriting.
            widgets
                .track_bytes
                .borrow_mut()
                .insert(item_id.clone(), tracks.iter().map(|track| (track.ino.clone(), track.bytes_downloaded.max(0) as u64)).collect());

            let subtitle = if let Some(reason) = widgets.failed.borrow().get(&item_id) {
                // Takes priority over the "N chapters, size" summary below: a failed batch may
                // still have left some chapters `Complete` (see `finish_track`'s ordering), but
                // *why the row is even still here* is the more useful thing to lead with.
                Some(tr_args("Download failed — {reason}", &[("reason", reason)]))
            } else if is_downloading {
                let batch = widgets.download_manager.batch_progress(&widgets.server_id, &item_id);
                let bytes: u64 = tracks.iter().map(|track| track.bytes_downloaded.max(0) as u64).sum();
                let speed = {
                    let speeds = widgets.speeds.borrow();
                    speeds
                        .get(&item_id)
                        .filter(|state| state.ema > 0.0 && state.last_instant.is_some_and(|at| at.elapsed() < SPEED_DISPLAY_WINDOW))
                        .map(|state| state.ema)
                };
                Some(downloading_subtitle(batch, bytes, speed))
            } else {
                let complete: Vec<_> = tracks.iter().filter(|track| track.status == DownloadStatus::Complete).collect();
                let size: u64 = complete.iter().map(|track| track.bytes_downloaded.max(0) as u64).sum();
                if complete.is_empty() {
                    None
                } else {
                    // TRANSLATORS: {chapters} is a chapter count such as "10 chapters", {size} the total size on disk such as "64.1 MB".
                    Some(tr_args("{chapters}, {size}", &[("chapters", &chapters_label(complete.len())), ("size", &format_bytes(size))]))
                }
            };

            let row = download_row(&item, is_downloading, &widgets.download_manager, &widgets.server_id, subtitle, &widgets.on_open, &widgets.toast_overlay);
            if is_downloading {
                widgets.live_rows.borrow_mut().insert(item_id.clone(), row.clone());
            }
            widgets.list_box.append(&row);
            any_row = true;
        }

        // Drop bookkeeping for items that left the list entirely (cleared/removed downloads).
        widgets.track_bytes.borrow_mut().retain(|id, _| all_ids.contains(id));
        widgets.speeds.borrow_mut().retain(|id, _| all_ids.contains(id));

        if read_failed {
            widgets.status_page.set_title(&tr("Couldn't read downloads"));
            widgets.status_page.set_description(Some(&tr("The app's local data couldn't be read. Restarting the app may help.")));
        } else {
            widgets.status_page.set_title(&tr("No downloads yet"));
            widgets.status_page.set_description(None);
        }
        widgets.status_page.set_visible(!any_row);
        widgets.scroller.set_visible(any_row);
        widgets.clear_all_button.set_sensitive(any_row);
    });
}

#[allow(clippy::too_many_arguments)]
fn download_row(
    item: &abs_storage::models::Item,
    is_downloading: bool,
    download_manager: &DownloadManager,
    server_id: &str,
    subtitle: Option<String>,
    on_open: &Rc<dyn Fn(crate::player::PlayRequest)>,
    toast_overlay: &adw::ToastOverlay,
) -> adw::ActionRow {
    const THUMBNAIL_SIZE: i32 = 48;
    let cover = crate::widgets::cover_image::CoverImage::new(THUMBNAIL_SIZE);
    cover.set_path(item.cover_cache_path.as_deref().map(std::path::Path::new));

    let row = adw::ActionRow::builder().title(&item.title).activatable(true).build();
    if let Some(text) = subtitle {
        row.set_subtitle(&text);
    }
    row.add_prefix(cover.widget());
    // Lands on Item Detail — the same surface every other item view (Home shelf cards, Library
    // grid) opens on tap, and where this item's own download progress/chapter glyphs live (see
    // widgets::download_progress, screens::item_detail's refresh_chapter_rows). The suffix's own
    // Stop/Remove button below claims its own click, so it never bubbles into this activation.
    row.connect_activated({
        let on_open = on_open.clone();
        let item_id = item.id.clone();
        let title = item.title.clone();
        let author = item.author.clone();
        move |_| on_open(crate::player::PlayRequest { item_id: item_id.clone(), title: title.clone(), author: author.clone() })
    });

    if is_downloading {
        let spinner = gtk4::Spinner::builder().spinning(true).valign(gtk4::Align::Center).build();
        // Stop, not cancel-delete: chapters that already completed stay downloaded (the row's own
        // subtitle switches to their "N chapters, size" summary once the batch winds down).
        let stop_button = gtk4::Button::builder().icon_name("process-stop-symbolic").tooltip_text(tr("Stop")).css_classes(["flat"]).valign(gtk4::Align::Center).build();
        stop_button.connect_clicked({
            let download_manager = download_manager.clone();
            let server_id = server_id.to_string();
            let item_id = item.id.clone();
            move |_| download_manager.cancel_item(&server_id, &item_id)
        });
        let box_ = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).build();
        box_.append(&spinner);
        box_.append(&stop_button);
        row.add_suffix(&box_);
    } else {
        let remove_button = gtk4::Button::builder().icon_name("user-trash-symbolic").tooltip_text(tr("Remove")).css_classes(["flat"]).valign(gtk4::Align::Center).build();
        remove_button.connect_clicked({
            let download_manager = download_manager.clone();
            let server_id = server_id.to_string();
            let item_id = item.id.clone();
            let toast_overlay = toast_overlay.clone();
            move |_| {
                let toast_overlay = toast_overlay.clone();
                download_manager.clear_item(&server_id, &item_id, move |result| {
                    if let Err(err) = result {
                        crate::error_reporting::report_background_error(&toast_overlay, &tr("Removing the download"), err);
                    }
                })
            }
        });
        row.add_suffix(&remove_button);
    }

    row
}

/// "1 chapter" / "10 chapters" — the completed-download subtitle's first half.
fn chapters_label(count: usize) -> String {
    ntr("{count} chapter", "{count} chapters", count as u32)
}

/// "5 B", "18.2 MB", "1.3 GB" — decimal units, matching how servers report Content-Length.
pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1e6)
    } else if bytes >= 1_000 {
        format!("{:.1} kB", bytes as f64 / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// The in-flight subtitle: chapter progress from the manager's batch ("3/10 chapters"), total
/// bytes so far, and — while samples are flowing — the smoothed speed. `pub(crate)` so
/// `widgets::download_progress::DownloadProgressStrip` (Item Detail/Player's own in-flight
/// indicator) reads the same numbers in the same words as this screen's own live row, rather
/// than inventing a second wording for the same data.
pub(crate) fn downloading_subtitle(batch: Option<(usize, usize)>, bytes: u64, speed: Option<f64>) -> String {
    let mut parts = Vec::new();
    if let Some((finished, total)) = batch {
        // TRANSLATORS: {finished} is how many chapters of this download are done, {total} how many it has in all, e.g. "3/10 chapters".
        parts.push(tr_args("{finished}/{total} chapters", &[("finished", &finished.to_string()), ("total", &total.to_string())]));
    }
    parts.push(format_bytes(bytes));
    if let Some(speed) = speed.filter(|rate| *rate > 0.0) {
        // TRANSLATORS: {speed} is a data size such as "2.1 MB"; the whole phrase is a download speed, "2.1 MB/s".
        parts.push(tr_args("{speed}/s", &[("speed", &format_bytes(speed as u64))]));
    }
    parts.join(" · ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::{pool, pump_until, test_paths};
    use abs_storage::models::{Account, Server};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn session_for(pool: &SqlitePool, server_url: &str) -> (abs_core::auth::Session, Server, Account) {
        let server_id = abs_storage::repo::servers::add(pool, server_url).await.unwrap();
        let account_id = abs_storage::repo::accounts::add(pool, &server_id, "jane", "token123", None).await.unwrap();
        let server = abs_storage::repo::servers::get(pool, &server_id).await.unwrap();
        let account = abs_storage::repo::accounts::get(pool, &account_id).await.unwrap();
        (abs_core::auth::Session::new(pool.clone(), &server, &account), server, account)
    }

    async fn insert_synced_item(pool: &SqlitePool, server_id: &str, item_id: &str, title: &str) {
        abs_storage::repo::libraries::upsert(
            pool,
            abs_storage::repo::libraries::UpsertLibrary { id: "lib-1", server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        abs_storage::repo::items::upsert(
            pool,
            abs_storage::repo::items::UpsertItem {
                id: item_id,
                server_id,
                library_id: "lib-1",
                title,
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
    }

    async fn mock_single_track_item(mock_server: &MockServer, item_id: &str, delay: Duration) {
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [{ "ino": "1", "duration": 5.0 }] }
            })))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}/file/1")))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "5").set_delay(delay).set_body_bytes(b"hello".to_vec()))
            .mount(mock_server)
            .await;
    }

    /// Two one-chapter-per-file tracks where the first completes immediately and the second is
    /// deliberately slow — the window a Stop press lands in after chapter 1 is already saved.
    async fn mock_two_track_item_slow_second(mock_server: &MockServer, item_id: &str, delay: Duration) {
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [{ "ino": "1", "duration": 5.0 }, { "ino": "2", "duration": 5.0 }],
                    "chapters": [
                        { "id": 0, "start": 0.0, "end": 5.0, "title": "Chapter 1" },
                        { "id": 1, "start": 5.0, "end": 10.0, "title": "Chapter 2" },
                    ],
                }
            })))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}/file/1")))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "5").set_body_bytes(b"hello".to_vec()))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}/file/2")))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "5").set_delay(delay).set_body_bytes(b"hello".to_vec()))
            .mount(mock_server)
            .await;
    }

    fn test_download_manager(pool: SqlitePool) -> DownloadManager {
        DownloadManager::new(pool, test_paths(), Box::new(abs_player::network_watch::UnknownNetworkMonitor), false)
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`.
    pub(crate) fn run_empty_state_renders_when_nothing_is_downloaded(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));

        let manager = test_download_manager(pool.clone());
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool, test_paths(), server, account, session, manager, app_window, Rc::new(|_| {}));
        let hooks = screen.test_hooks();

        pump_until(|| hooks.status_page.is_visible(), Duration::from_secs(5));
        assert!(hooks.status_page.is_visible(), "empty state should render with nothing downloaded");
        assert!(!hooks.scroller.is_visible());
    }

    /// A download in progress shows a row with a cancel button, not a remove button; canceling it
    /// removes the row entirely (nothing ever completed for this item).
    pub(crate) fn run_in_progress_download_shows_a_cancel_row(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_single_track_item(&mock_server, "item-1", Duration::from_secs(2)));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let manager = test_download_manager(pool.clone());
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account, session.clone(), manager.clone(), app_window, Rc::new(|_| {}));
        let hooks = screen.test_hooks();

        manager.start_download(session, "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(5));
        assert!(hooks.scroller.is_visible(), "an in-flight download should show a row");

        manager.cancel_item(&server.id, "item-1");
        pump_until(|| hooks.status_page.is_visible(), Duration::from_secs(10));
        assert!(hooks.status_page.is_visible(), "canceling the only (never-completed) download should return to the empty state");
    }

    /// An in-flight download's row shows live progress (chapter fraction from the batch, bytes,
    /// speed while samples flow) in its subtitle, not just a spinner; once complete, the
    /// subtitle becomes the static "1 chapter, 5 B" summary.
    pub(crate) fn run_in_progress_download_shows_progress_and_speed(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_single_track_item(&mock_server, "item-1", Duration::from_secs(2)));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let manager = test_download_manager(pool.clone());
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account, session.clone(), manager.clone(), app_window, Rc::new(|_| {}));
        let hooks = screen.test_hooks();

        manager.start_download(session, "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(5));

        let row = hooks.list_box.row_at_index(0).unwrap().downcast::<adw::ActionRow>().unwrap();
        let subtitle = row.subtitle().unwrap();
        assert!(subtitle.contains("0/1 chapters"), "in-flight subtitle should show the batch's chapter progress, got: {subtitle}");
        assert!(subtitle.contains("B"), "in-flight subtitle should show bytes so far, got: {subtitle}");

        // Pure formatting, asserted alongside so the units stay pinned (no GTK involved).
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.2 MB");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(downloading_subtitle(Some((3, 10)), 18_900_000, Some(2_200_000.0)), "3/10 chapters · 18.9 MB · 2.2 MB/s");
        assert_eq!(chapters_label(1), "1 chapter");
        assert_eq!(chapters_label(10), "10 chapters");

        pump_until(
            || {
                hooks
                    .list_box
                    .row_at_index(0)
                    .and_then(|row| row.downcast::<adw::ActionRow>().ok())
                    .and_then(|row| row.subtitle())
                    .is_some_and(|subtitle| subtitle.contains("chapter") && subtitle.contains(","))
            },
            Duration::from_secs(10),
        );
        let subtitle = hooks.list_box.row_at_index(0).unwrap().downcast::<adw::ActionRow>().unwrap().subtitle().unwrap();
        assert_eq!(subtitle, "1 chapter, 5 B", "completed subtitle is the static chapters+size summary");
    }

    /// Pressing Stop mid-batch is a graceful end: the chapters that already completed stay
    /// downloaded (the row settles into their summary), the job winds down, and the state is
    /// `Stopped` — deliberately not a failure.
    pub(crate) fn run_stop_keeps_completed_chapters_and_stops_the_job(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item_slow_second(&mock_server, "item-1", Duration::from_secs(3)));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let manager = test_download_manager(pool.clone());
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account, session.clone(), manager.clone(), app_window, Rc::new(|_| {}));
        let hooks = screen.test_hooks();

        let events: Rc<RefCell<Vec<DownloadEvent>>> = Rc::new(RefCell::new(Vec::new()));
        manager.add_listener({
            let events = events.clone();
            move |event| events.borrow_mut().push(event.clone())
        });

        manager.start_download(session, "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);

        // Wait until chapter 1's track is fully downloaded, then stop while chapter 2 lags.
        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|row| row.status == DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );
        manager.cancel_item(&server.id, "item-1");

        pump_until(
            || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Stopped, .. })),
            Duration::from_secs(10),
        );

        // The completed chapter survived the stop…
        let track_1 = runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().unwrap();
        assert_eq!(track_1.status, DownloadStatus::Complete, "a stopped download keeps its completed chapters");
        // …and the row settles into the completed-summary view instead of disappearing.
        pump_until(
            || {
                hooks
                    .list_box
                    .row_at_index(0)
                    .and_then(|row| row.downcast::<adw::ActionRow>().ok())
                    .and_then(|row| row.subtitle())
                    .is_some_and(|subtitle| subtitle == "1 chapter, 5 B")
            },
            Duration::from_secs(10),
        );
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A failed Remove must say so rather
    /// than leave a row that silently ignores the tap, and a failed read of the downloads list
    /// must not claim there are no downloads. Closing the pool makes both fail for real.
    pub(crate) fn run_database_failures_are_reported(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_single_track_item(&mock_server, "item-1", Duration::ZERO));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let manager = test_download_manager(pool.clone());
        manager.start_download(session.clone(), "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );

        // `AdwToastOverlay` defers toasts while unmapped.
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account.clone(), session.clone(), manager.clone(), app_window.clone(), Rc::new(|_| {}));
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));
        let hooks = screen.test_hooks();
        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(5));

        runtime.block_on(pool.close());

        let row = hooks.list_box.row_at_index(0).unwrap();
        crate::widgets::find_descendant::<gtk4::Button>(row.upcast_ref()).expect("the row's Remove button").emit_clicked();
        pump_until(|| crate::test_support::any_label_reads(&screen.root, "Removing the download failed — try again"), Duration::from_secs(5));
        assert!(
            crate::test_support::any_label_reads(&screen.root, "Removing the download failed — try again"),
            "a failed Remove must be reported"
        );
        app_window.destroy();

        // A fresh screen over the same (closed) pool: its first read fails.
        let screen = build(pool.clone(), test_paths(), server, account, session, manager, adw::ApplicationWindow::builder().build(), Rc::new(|_| {}));
        let hooks = screen.test_hooks();
        pump_until(|| hooks.status_page.is_visible(), Duration::from_secs(5));
        assert_eq!(hooks.status_page.title(), "Couldn't read downloads", "a failed read must not claim there are no downloads");
    }

    /// A completed download shows a row with a remove button; removing it deletes the row and the
    /// underlying rows/files (verified indirectly via `downloaded_item_ids` going back to empty).
    pub(crate) fn run_completed_download_can_be_removed(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_single_track_item(&mock_server, "item-1", Duration::ZERO));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let manager = test_download_manager(pool.clone());
        manager.start_download(session.clone(), "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );

        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account, session, manager.clone(), app_window, Rc::new(|_| {}));
        let hooks = screen.test_hooks();
        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(5));

        manager.clear_item(&server.id, "item-1", |result| result.unwrap());
        pump_until(|| hooks.status_page.is_visible(), Duration::from_secs(10));
        assert!(hooks.status_page.is_visible(), "removing the only completed download should return to the empty state");
        assert!(runtime.block_on(abs_storage::repo::download_tracks::list_for_item(&pool, &server.id, "item-1")).unwrap().is_empty());
    }

    /// Regression test for "a Downloads row is otherwise dead — no way back to the book it's
    /// for": tapping a row must report that item's `PlayRequest` via `on_open`, and the Remove
    /// button (a `GtkButton` inside the row's suffix) must claim its own click rather than
    /// bubbling into the row's own activation.
    pub(crate) fn run_tapping_a_row_opens_it_and_remove_does_not(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_single_track_item(&mock_server, "item-1", Duration::ZERO));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let manager = test_download_manager(pool.clone());
        manager.start_download(session.clone(), "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );

        let opened: Rc<RefCell<Vec<crate::player::PlayRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account, session, manager, app_window, {
            let opened = opened.clone();
            Rc::new(move |request| opened.borrow_mut().push(request))
        });
        let hooks = screen.test_hooks();
        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(5));

        let row = hooks.list_box.row_at_index(0).unwrap();
        row.emit_by_name::<()>("activated", &[]);
        assert_eq!(opened.borrow().len(), 1, "tapping the row should report this item's PlayRequest");
        assert_eq!(opened.borrow()[0].item_id, "item-1");
        assert_eq!(opened.borrow()[0].title, "Test Item");

        // The Remove button (inside the row's suffix) must claim its own click, not bubble into
        // the row's own activation.
        let remove_button = crate::widgets::find_descendant::<gtk4::Button>(row.upcast_ref()).expect("the completed row's Remove button");
        remove_button.emit_clicked();
        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().is_none(),
            Duration::from_secs(5),
        );
        assert!(
            runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().is_none(),
            "Remove should have actually run"
        );
        assert_eq!(opened.borrow().len(), 1, "clicking Remove must not also open the row");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The "Clear all downloads" button
    /// starts disabled with nothing downloaded, and becomes enabled once something completes.
    pub(crate) fn run_clear_all_button_is_disabled_until_something_is_downloaded(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_single_track_item(&mock_server, "item-1", Duration::ZERO));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let manager = test_download_manager(pool.clone());
        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account, session.clone(), manager.clone(), app_window, Rc::new(|_| {}));
        let hooks = screen.test_hooks();

        pump_until(|| hooks.status_page.is_visible(), Duration::from_secs(5));
        assert!(!hooks.clear_all_button.is_sensitive(), "nothing to clear yet");

        manager.start_download(session, "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        pump_until(|| hooks.clear_all_button.is_sensitive(), Duration::from_secs(10));
        assert!(hooks.clear_all_button.is_sensitive(), "a completed download should enable the button");
    }

    fn find_message_dialog() -> Option<gtk4::MessageDialog> {
        gtk4::Window::list_toplevels().into_iter().find_map(|window| window.downcast::<gtk4::MessageDialog>().ok())
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Cancelling the confirmation dialog
    /// must leave every downloaded row, DB row, and file exactly as it was.
    pub(crate) fn run_clear_all_cancelled_leaves_downloads_intact(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_single_track_item(&mock_server, "item-1", Duration::ZERO));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let manager = test_download_manager(pool.clone());
        manager.start_download(session.clone(), "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );

        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account, session, manager.clone(), app_window, Rc::new(|_| {}));
        let hooks = screen.test_hooks();
        pump_until(|| hooks.clear_all_button.is_sensitive(), Duration::from_secs(5));

        hooks.clear_all_button.emit_clicked();
        pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
        find_message_dialog().unwrap().response(gtk4::ResponseType::Cancel);
        pump_until(|| find_message_dialog().is_none(), Duration::from_secs(5));

        // Give any (wrongly fired) clearing work a chance to run, then assert nothing changed.
        pump_until(|| false, Duration::from_millis(300));
        assert!(hooks.scroller.is_visible(), "a cancelled confirmation must leave the row in place");
        assert!(!runtime.block_on(abs_storage::repo::download_tracks::list_for_item(&pool, &server.id, "item-1")).unwrap().is_empty(), "a cancelled confirmation must not touch the DB rows");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Confirming "Clear all downloads"
    /// removes every downloaded item's rows and files, across every item — not just one.
    pub(crate) fn run_clear_all_confirmed_removes_every_download_and_its_files(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_single_track_item(&mock_server, "item-1", Duration::ZERO));
        runtime.block_on(mock_single_track_item(&mock_server, "item-2", Duration::ZERO));
        let (session, server, account) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item 1"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Test Item 2"));

        let manager = test_download_manager(pool.clone());
        manager.start_download(session.clone(), "item-1".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        manager.start_download(session.clone(), "item-2".to_string(), abs_core::downloads::DownloadScope::EntireBook, 0);
        pump_until(
            || {
                [("item-1", &pool), ("item-2", &pool)].iter().all(|(item_id, pool)| {
                    runtime.block_on(abs_storage::repo::download_tracks::get(pool, &server.id, item_id, "1")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false)
                })
            },
            Duration::from_secs(10),
        );
        let file_path = runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().unwrap().file_path;
        assert!(std::path::Path::new(&file_path).exists(), "the download should have actually written a file");

        let app_window = adw::ApplicationWindow::builder().build();
        let screen = build(pool.clone(), test_paths(), server.clone(), account, session, manager.clone(), app_window, Rc::new(|_| {}));
        let hooks = screen.test_hooks();
        pump_until(|| hooks.clear_all_button.is_sensitive(), Duration::from_secs(5));

        hooks.clear_all_button.emit_clicked();
        pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
        find_message_dialog().unwrap().response(gtk4::ResponseType::Ok);

        pump_until(|| hooks.status_page.is_visible(), Duration::from_secs(10));
        assert!(hooks.status_page.is_visible(), "clearing everything should return to the empty state");
        assert!(runtime.block_on(abs_storage::repo::download_tracks::list_for_item(&pool, &server.id, "item-1")).unwrap().is_empty());
        assert!(runtime.block_on(abs_storage::repo::download_tracks::list_for_item(&pool, &server.id, "item-2")).unwrap().is_empty());
        pump_until(|| !std::path::Path::new(&file_path).exists(), Duration::from_secs(5));
        assert!(!std::path::Path::new(&file_path).exists(), "the file on disk should be deleted too");
    }
}
