//! Orchestrates the download pipeline's app-level concerns — queueing, concurrency limiting,
//! per-track cancellation, and event publishing — that `abs_core::download_tracks::download_track`
//! deliberately doesn't own (see that module's doc comment). Structured like
//! `player::PlayerController`: an `Rc<RefCell<Inner>>` driven via `glib::spawn_future_local`, with
//! the same dependency-injected shape (`abs_player::network_watch::NetworkMonitor`) so tests can
//! run without a real D-Bus/network stack.
//!
//! Never imports `abs_api` directly: every network call flows through `abs_core::streaming` (to
//! discover tracks/chapters the first time an item is downloaded without ever having been played)
//! and `abs_core::download_tracks` (the actual per-track fetch) — same boundary every other
//! screen/controller in this crate already respects.
//!
//! Deliberately does not persist or restore in-flight batches across an app restart: a download
//! killed mid-transfer leaves a resumable `download_tracks` row (per-track state lives in
//! `abs-storage`, not here), but resuming it automatically on the next launch is a product decision
//! for a later pass (the Downloads screen), not something this orchestrator does on its own.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use adw::glib;
use sqlx::SqlitePool;

use abs_core::auth::Session;
use abs_core::download_tracks::TrackDownloadOutcome;
use abs_core::downloads::DownloadScope;
use abs_player::network_watch::NetworkMonitor;
use abs_storage::AppPaths;

use crate::i18n::{tr, tr_args};

/// How many tracks may download concurrently across the whole app, regardless of how many items
/// are queued — bounded so downloads don't starve sync/cover/playback traffic sharing the same
/// connection. Not user-configurable (yet); a fixed, conservative default.
const MAX_CONCURRENT_DOWNLOADS: usize = 2;

/// The minimum gap between two `DownloadEvent::TrackProgress` publishes for one track — see
/// `spawn_track_download`'s `on_progress`. Matches `screens::downloads::LABEL_UPDATE_INTERVAL`
/// (no point publishing faster than the fastest listener actually repaints). `pub(crate)` only so
/// that comment cross-reference stays meaningful; nothing outside this module needs to read it.
pub(crate) const PROGRESS_EVENT_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemDownloadState {
    /// Nothing in flight for this item — either it was never started, finished being cleared, or
    /// every in-flight track was canceled before the whole batch finished.
    Idle,
    Downloading,
    Complete,
    /// The user pressed Stop on an in-flight download: in-flight tracks were canceled and the
    /// chapters that had already completed stay downloaded (their files and rows are untouched) —
    /// a user-driven end, deliberately distinct from `Failed` (where a track itself errored).
    Stopped,
    /// Carries a human-readable reason (the first failing track's `TrackDownloadOutcome::Failed`
    /// message, or whatever stopped the batch before any track was even attempted — a dead
    /// connection, a resolve failure, "waiting for a non-metered connection") so a listener can
    /// show *why*, not just that it failed. Before this, the reason existed
    /// (`TrackDownloadOutcome::Failed(String)`, persisted via `mark_failed`) but nothing surfaced
    /// it: a scope-row tap toasted "Download started" and the download button's icon quietly
    /// reverted to idle a moment later, with no way to tell "waiting for Wi-Fi" apart from a real
    /// error.
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum DownloadEvent {
    TrackProgress {
        item_id: String,
        ino: String,
        bytes_downloaded: u64,
        /// Not read by any current listener (every consumer derives its own total from cached
        /// track sizes instead) — kept on the event since it's cheap to carry and a future
        /// progress display may want it directly rather than re-deriving it.
        #[allow(dead_code)]
        total_bytes: Option<u64>,
    },
    ItemStateChanged {
        item_id: String,
        state: ItemDownloadState,
    },
}

/// Returns whether to keep listening: `false` (its screen is gone) drops the listener.
type EventListener = Box<dyn Fn(&DownloadEvent) -> bool>;

/// Bookkeeping for one item's in-flight download batch: how many tracks were queued, how each one
/// finished so far, and every in-flight track's cooperative cancel flag so `cancel_item` can stop
/// them without tearing down the whole manager. Removed from `Inner::batches` once every track in
/// it has finished (successfully, failed, or canceled).
struct ItemBatch {
    total: usize,
    completed: usize,
    failed: usize,
    canceled: usize,
    cancel_flags: Vec<Rc<Cell<bool>>>,
    /// The first failing track's reason — kept, not the latest, so a batch where one track fails
    /// early and others keep going still reports the failure that actually explains why the item
    /// ended up `Failed` rather than whichever happened to finish last.
    first_failure_reason: Option<String>,
}

impl ItemBatch {
    fn finished(&self) -> usize {
        self.completed + self.failed + self.canceled
    }
}

struct Inner {
    pool: SqlitePool,
    paths: AppPaths,
    network_monitor: Box<dyn NetworkMonitor>,
    wifi_only: bool,
    semaphore: Rc<tokio::sync::Semaphore>,
    /// Each with the server whose events it wants (`None`: every server's) — see
    /// [`DownloadManager::scoped_to`].
    listeners: RefCell<Vec<(Option<String>, EventListener)>>,
    /// Listeners registered from inside a publish; added once it is over.
    listeners_added_meanwhile: RefCell<Vec<(Option<String>, EventListener)>>,
    /// Keyed by `(server_id, item_id)` rather than just `item_id` — the same item id is only ever
    /// meaningful within one server, but nothing stops two different servers from happening to
    /// reuse the same id, and this manager is shared for the app's whole lifetime, across account
    /// switches (see [`app_wide`]).
    batches: HashMap<(String, String), ItemBatch>,
}

impl Inner {
    /// Delivers `event`, about a download for `server_id`, to the listeners that want that
    /// server's events. `None` (tests only) reaches every listener.
    fn publish(&self, server_id: Option<&str>, event: DownloadEvent) {
        self.listeners.borrow_mut().retain(|(scope, listener)| match (scope, server_id) {
            (Some(scope), Some(server_id)) if scope != server_id => true,
            _ => listener(&event),
        });
        let mut added = self.listeners_added_meanwhile.borrow_mut();
        self.listeners.borrow_mut().append(&mut added);
    }
}

#[derive(Clone)]
pub struct DownloadManager {
    inner: Rc<RefCell<Inner>>,
    /// The server this handle's listeners hear about (`None`: all) — see [`Self::scoped_to`].
    scope: Option<String>,
    /// While set, this handle's listeners are dropped once it can't be upgraded — the shell
    /// that registered them is gone (see [`Self::scoped_to`]).
    alive: Option<std::rc::Weak<()>>,
}

thread_local! {
    static APP_WIDE: RefCell<Option<DownloadManager>> = const { RefCell::new(None) };
}

/// The one manager every shell shares, created by `create` the first time. Downloads keep running
/// across a server switch (switching back shows them in progress again), so the manager can't be
/// rebuilt with the shell. Each shell takes a handle [`DownloadManager::scoped_to`] its server.
pub fn app_wide(create: impl FnOnce() -> DownloadManager) -> DownloadManager {
    APP_WIDE.with(|cell| cell.borrow_mut().get_or_insert_with(create).clone())
}

impl DownloadManager {
    pub fn new(pool: SqlitePool, paths: AppPaths, network_monitor: Box<dyn NetworkMonitor>, wifi_only: bool) -> Self {
        Self {
            inner: Rc::new(RefCell::new(Inner {
                pool,
                paths,
                network_monitor,
                wifi_only,
                semaphore: Rc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
                listeners: RefCell::new(Vec::new()),
                listeners_added_meanwhile: RefCell::new(Vec::new()),
                batches: HashMap::new(),
            })),
            scope: None,
            alive: None,
        }
    }

    /// A handle on the same manager whose listeners only hear about `server_id`'s downloads, and
    /// whose [`Self::any_in_flight`] only counts them, and that live only as long as `alive` does
    /// — what a shell, showing one server for as long as it exists, uses.
    pub fn scoped_to(&self, server_id: &str, alive: &Rc<()>) -> Self {
        Self { inner: self.inner.clone(), scope: Some(server_id.to_string()), alive: Some(Rc::downgrade(alive)) }
    }

    /// Registers a permanent event listener, notified for the manager's whole lifetime — same
    /// "no corresponding unregister" shape as `PlayerController::add_listener` (nothing needs to
    /// stop listening once registered).
    /// Delivers `event` to the listeners as if a download had produced it — for screen tests that
    /// need a state change without running a download.
    #[cfg(test)]
    pub(crate) fn publish_for_test(&self, event: DownloadEvent) {
        self.inner.borrow().publish(None, event);
    }

    pub fn add_listener(&self, listener: impl Fn(&DownloadEvent) + 'static) {
        self.add_scoped_listener(move |event| {
            listener(event);
            true
        });
    }

    /// Like `add_listener`, for a listener tied to a screen that comes and goes (Item Detail and
    /// the Player are built afresh per visit): it returns `false` once that screen is gone and is
    /// then dropped, instead of staying registered — and keeping the screen's widgets alive — for
    /// the manager's whole lifetime.
    pub fn add_scoped_listener(&self, listener: impl Fn(&DownloadEvent) -> bool + 'static) {
        let inner = self.inner.borrow();
        let alive = self.alive.clone();
        let listener: EventListener = Box::new(move |event| alive.as_ref().is_none_or(|alive| alive.upgrade().is_some()) && listener(event));
        let listener = (self.scope.clone(), listener);
        let added_now = match inner.listeners.try_borrow_mut() {
            Ok(mut listeners) => {
                listeners.push(listener);
                None
            }
            Err(_) => Some(listener),
        };
        if let Some(listener) = added_now {
            inner.listeners_added_meanwhile.borrow_mut().push(listener);
        }
    }

    /// This item's in-flight batch's progress as `(finished tracks, total tracks)`. `finished`
    /// ticks per track outcome (completed, failed, or canceled) — not just when the whole batch
    /// ends, which is the only thing `ItemStateChanged` reports — so the Downloads screen's
    /// subtitle can advance live while chapters land. `None` when nothing is in flight.
    pub fn batch_progress(&self, server_id: &str, item_id: &str) -> Option<(usize, usize)> {
        let inner = self.inner.borrow();
        let batch = inner.batches.get(&(server_id.to_string(), item_id.to_string()))?;
        Some((batch.finished(), batch.total))
    }

    /// Whether a download batch is currently in flight for this item — the player's download
    /// dropdown uses this to offer Stop exactly while there's something to stop.
    pub fn is_downloading(&self, server_id: &str, item_id: &str) -> bool {
        self.inner.borrow().batches.contains_key(&(server_id.to_string(), item_id.to_string()))
    }

    /// The items of `server_id` with a download in flight — for a Downloads screen built while
    /// they were already running (after switching back to their server), which never saw them
    /// start.
    pub fn in_flight_items(&self, server_id: &str) -> Vec<String> {
        self.inner.borrow().batches.keys().filter(|(server, _)| server == server_id).map(|(_, item)| item.clone()).collect()
    }

    /// Whether *anything* is downloading right now, across every item — the Downloads tab's own
    /// attention-dot uses this so "something is running in the background" stays visible from
    /// Home/Library too, not just from whichever screen happened to start it.
    pub fn any_in_flight(&self) -> bool {
        let inner = self.inner.borrow();
        match &self.scope {
            Some(server_id) => inner.batches.keys().any(|(server, _)| server == server_id),
            None => !inner.batches.is_empty(),
        }
    }

    /// Whether this device can currently tell a metered connection from an unmetered one. When
    /// it can't (no NetworkManager, or it doesn't know), Wi-Fi-only never blocks anything, and
    /// Settings says so rather than offering a switch that silently does nothing.
    pub fn can_detect_metered(&self) -> bool {
        self.inner.borrow().network_monitor.is_metered().is_some()
    }

    /// Bytes free on the filesystem holding the downloads directory — the number the download
    /// sheet's size estimates are compared against. The downloads dir may not exist yet (it's
    /// created lazily at first download), so its parent is probed first; either missing means
    /// "unknown", and the sheet must treat unknown as unrestricted, never as empty.
    pub fn free_space_bytes(&self) -> Option<u64> {
        let paths = self.inner.borrow().paths.clone();
        abs_storage::paths::free_space_bytes(&paths.downloads_dir()).or_else(|| abs_storage::paths::free_space_bytes(paths.data_dir()))
    }

    pub fn set_wifi_only(&self, value: bool) {
        self.inner.borrow_mut().wifi_only = value;
    }

    /// What `set_wifi_only` last applied — for asserting that the Settings screen's switch
    /// actually reaches the manager.
    #[cfg(test)]
    pub(crate) fn wifi_only(&self) -> bool {
        self.inner.borrow().wifi_only
    }

    /// Resolves `scope` (relative to `current_chapter_index`) into the set of tracks it touches and
    /// starts fetching whichever of them aren't already complete. Fetches and caches track/chapter
    /// metadata first if it isn't already cached locally (e.g. this item has never been played), so
    /// downloading never requires having played the item first.
    pub fn start_download(&self, session: Session, item_id: String, scope: DownloadScope, current_chapter_index: usize) {
        // Offline mode: nothing may reach the server. The download buttons already say so; this
        // is the backstop for any other caller.
        if session.is_offline() {
            tracing::info!(item_id, "offline mode is on; not starting a download");
            self.inner.borrow().publish(Some(session.server_id()), DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Failed(tr("offline mode is on")) });
            return;
        }
        let inner_rc = self.inner.clone();
        glib::spawn_future_local(async move {
            let pool = inner_rc.borrow().pool.clone();
            let server_id = session.server_id().to_string();

            let mut tracks = abs_core::tracks::cached_tracks(&pool, &server_id, &item_id).await.unwrap_or_default();
            let mut chapters = abs_core::chapters::cached_chapters(&pool, &server_id, &item_id).await.unwrap_or_default();

            if tracks.is_empty() {
                // Asked at resolve time, not captured earlier — same "never let a captured token
                // (or connection) go stale across a long-running operation" posture as
                // `PlayerController::start`.
                let access_token = session.access_token().await;
                let connection = match session.connection_target().await {
                    Ok(connection) => connection,
                    Err(err) => {
                        tracing::warn!(%err, item_id, "couldn't load the server's connection settings");
                        let err = err.to_string();
                        // TRANSLATORS: {error} is the error message from the underlying library (not translated). This is the reason shown after "Download failed — ".
                        let reason = tr_args("couldn't load the server's connection settings: {error}", &[("error", &err)]);
                        inner_rc.borrow().publish(Some(&server_id), DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Failed(reason) });
                        return;
                    }
                };
                match abs_core::streaming::resolve_stream_target(&connection, &access_token, &item_id).await {
                    Ok(target) => {
                        if let Err(err) = abs_core::tracks::sync_item_tracks(&pool, &server_id, &item_id, &target.tracks).await {
                            tracing::warn!(%err, item_id, "couldn't persist tracks locally");
                        }
                        if let Err(err) = abs_core::tracks::fill_file_facts(&pool, &server_id, &item_id, &target.file_facts).await {
                            tracing::debug!(%err, item_id, "couldn't keep the audio files' encoding facts");
                        }
                        if let Err(err) = abs_core::chapters::sync_item_chapters(&pool, &server_id, &item_id, &target.chapters).await {
                            tracing::warn!(%err, item_id, "couldn't persist chapters locally");
                        }
                        tracks = abs_core::tracks::cached_tracks(&pool, &server_id, &item_id).await.unwrap_or_default();
                        chapters = abs_core::chapters::cached_chapters(&pool, &server_id, &item_id).await.unwrap_or_default();
                    }
                    Err(err) => {
                        tracing::warn!(%err, item_id, "couldn't resolve tracks for download");
                        let reason = match err {
                            abs_core::CoreError::NoPlayableAudio(_) => tr("this item has no audio to download"),
                            other => other.to_string(),
                        };
                        inner_rc.borrow().publish(Some(&server_id), DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Failed(reason) });
                        return;
                    }
                }
            }

            if tracks.is_empty() {
                let reason = tr("the server reported no audio files for this item");
                inner_rc.borrow().publish(Some(&server_id), DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Failed(reason) });
                return;
            }

            // An item with no chapter markers at all (rare, but real) has nothing for
            // `resolve_scope`/`tracks_needed_for_chapters` to work against — every scope
            // degenerates to "the whole book" in that case, since there's no finer-grained unit to
            // select from.
            let needed: BTreeSet<String> = if chapters.is_empty() {
                tracks.iter().map(|t| t.ino.clone()).collect()
            } else {
                let chapter_indices = abs_core::downloads::resolve_scope(scope, current_chapter_index, chapters.len());
                abs_core::download_tracks::tracks_needed_for_chapters(&tracks, &chapters, &chapter_indices)
            };

            // Filter out tracks already downloaded — `download_track` itself is idempotent on a
            // completed track, but filtering here keeps this batch's `total` (and therefore its
            // progress bookkeeping) accurate rather than counting cache hits as "in flight". A
            // track only counts as downloaded if its file checks out, so a download re-fetches
            // a damaged one rather than reporting the item complete over it.
            let pending_inos = abs_core::download_tracks::tracks_to_fetch(&pool, &server_id, &item_id, needed).await;

            if pending_inos.is_empty() {
                inner_rc.borrow().publish(Some(&server_id), DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Complete });
                return;
            }

            let cancel_flags: Vec<Rc<Cell<bool>>> = pending_inos.iter().map(|_| Rc::new(Cell::new(false))).collect();
            {
                let mut inner = inner_rc.borrow_mut();
                inner.batches.insert(
                    (server_id.clone(), item_id.clone()),
                    ItemBatch { total: pending_inos.len(), completed: 0, failed: 0, canceled: 0, cancel_flags: cancel_flags.clone(), first_failure_reason: None },
                );
                inner.publish(Some(&server_id), DownloadEvent::ItemStateChanged { item_id: item_id.clone(), state: ItemDownloadState::Downloading });
            }

            for (ino, cancel_flag) in pending_inos.into_iter().zip(cancel_flags) {
                Self::spawn_track_download(inner_rc.clone(), session.clone(), item_id.clone(), ino, cancel_flag);
            }
        });
    }

    /// Runs one track's download under the shared concurrency semaphore. `cancel_flag` is this
    /// track's own slot in its batch's `cancel_flags`, flipped by `cancel_item`.
    fn spawn_track_download(inner_rc: Rc<RefCell<Inner>>, session: Session, item_id: String, ino: String, cancel_flag: Rc<Cell<bool>>) {
        glib::spawn_future_local(async move {
            let semaphore = inner_rc.borrow().semaphore.clone();
            // A permit acquired before the metered check below matters: it's what actually bounds
            // how many attempts (including ones that immediately bail out for being on a metered
            // connection) can be "in flight" at once, keeping the accounting in `finish_track`
            // consistent regardless of why a track didn't proceed.
            let permit = semaphore.acquire().await.expect("semaphore is never closed");

            let (pool, paths, wifi_only, is_metered) = {
                let inner = inner_rc.borrow();
                (inner.pool.clone(), inner.paths.clone(), inner.wifi_only, inner.network_monitor.is_metered())
            };
            let server_id = session.server_id().to_string();

            // An unknown/undeterminable network type must never block a download — only a
            // *known* metered connection does, and only when the setting asks for it. This is
            // checked once per track start, not continuously (see this module's doc comment).
            if wifi_only && is_metered == Some(true) {
                drop(permit);
                Self::finish_track(&inner_rc, &server_id, &item_id, TrackDownloadOutcome::Failed(tr("waiting for a non-metered connection")));
                return;
            }

            let cancel_check = {
                let flag = cancel_flag.clone();
                move || flag.get()
            };

            let progress_item_id = item_id.clone();
            let progress_ino = ino.clone();
            let progress_inner = inner_rc.clone();
            // `download_track` calls this once per network chunk (typically 8-16 KB), which can
            // be many times a second — every listener (the Downloads screen, Item Detail's
            // download button, the scope menu) would otherwise redo its own bookkeeping that
            // often for no visible benefit. Throttled here at the source, once, rather than in
            // each listener: the Downloads screen already throttles its own label repaint
            // (`LABEL_UPDATE_INTERVAL`) downstream of this, but that only saved the repaint, not
            // the dispatch or every listener's per-event work. Always let the *last* chunk of a
            // track through regardless of timing, so a fast/small track's completion is never
            // dropped by the throttle window.
            let mut last_published = std::time::Instant::now() - PROGRESS_EVENT_MIN_INTERVAL;
            let progress_server_id = server_id.clone();
            let on_progress = move |downloaded: u64, total: Option<u64>| {
                let is_last_chunk = total.is_some_and(|total| downloaded >= total);
                if !is_last_chunk && last_published.elapsed() < PROGRESS_EVENT_MIN_INTERVAL {
                    return;
                }
                last_published = std::time::Instant::now();
                progress_inner.borrow().publish(Some(&progress_server_id), DownloadEvent::TrackProgress {
                    item_id: progress_item_id.clone(),
                    ino: progress_ino.clone(),
                    bytes_downloaded: downloaded,
                    total_bytes: total,
                });
            };

            // Asked fresh for each track, not carried over from `start_download`'s own call — a
            // batch of many tracks (an "entire book" download) can easily outlast a short-lived
            // access token, and `Session::access_token` is exactly the "refresh if needed"
            // primitive `PlayerController` already relies on for the same reason. The
            // connection is asked the same way: a settings change applies to the next track.
            let access_token = session.access_token().await;
            let connection = match session.connection_target().await {
                Ok(connection) => connection,
                Err(err) => {
                    let err = err.to_string();
                    // TRANSLATORS: {error} is the error message from the underlying library (not translated). This is the reason shown after "Download failed — ".
                    let reason = tr_args("the server's connection settings are gone: {error}", &[("error", &err)]);
                    abs_storage::repo::download_tracks::mark_failed(&pool, &server_id, &item_id, &ino, &reason).await.ok();
                    Self::finish_track(&inner_rc, &server_id, &item_id, TrackDownloadOutcome::Failed(reason));
                    return;
                }
            };
            let outcome = abs_core::download_tracks::download_track(&paths, &pool, &connection, &access_token, &server_id, &item_id, &ino, on_progress, &cancel_check)
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(%err, item_id = %item_id, ino = %ino, "download_track returned an error");
                    TrackDownloadOutcome::Failed(err.to_string())
                });
            if let TrackDownloadOutcome::Failed(reason) = &outcome {
                // What the server says the file is, for a bug report: a codec or bit rate that
                // explains an unusually large or odd file.
                let encoding = abs_core::tracks::describe_file(&pool, &server_id, &item_id, &ino).await;
                tracing::info!(item_id = %item_id, ino = %ino, %reason, encoding = encoding.as_deref().unwrap_or("unknown"), "a track download failed");
            }

            drop(permit);
            Self::finish_track(&inner_rc, &server_id, &item_id, outcome);
        });
    }

    /// Records one track's outcome against its batch and, once every track in the batch has
    /// finished, publishes the item's overall state and removes the batch.
    fn finish_track(inner_rc: &Rc<RefCell<Inner>>, server_id: &str, item_id: &str, outcome: TrackDownloadOutcome) {
        let mut inner = inner_rc.borrow_mut();
        let key = (server_id.to_string(), item_id.to_string());
        let Some(batch) = inner.batches.get_mut(&key) else { return };

        match outcome {
            TrackDownloadOutcome::Completed => batch.completed += 1,
            TrackDownloadOutcome::Failed(reason) => {
                batch.failed += 1;
                batch.first_failure_reason.get_or_insert(reason);
            }
            TrackDownloadOutcome::Canceled => batch.canceled += 1,
        }

        if batch.finished() < batch.total {
            return;
        }

        let (failed, canceled) = (batch.failed, batch.canceled);
        let first_failure_reason = batch.first_failure_reason.clone();
        inner.batches.remove(&key);

        let state = if failed > 0 {
            // `first_failure_reason` is only ever `None` here if every failing track somehow hit
            // `finish_track` without going through the `Failed(reason)` arm above — shouldn't
            // happen, but a generic message beats a blank one if it ever did.
            ItemDownloadState::Failed(first_failure_reason.unwrap_or_else(|| tr("the download failed")))
        } else if canceled > 0 {
            // The user stopped the job — whatever completed chapters exist are kept (their files
            // and rows were never touched by cancellation), and a stop with nothing completed
            // simply leaves nothing behind. Never surfaced as a failure: pressing Stop isn't an
            // error, and the completed part is exactly what was asked to be saved.
            ItemDownloadState::Stopped
        } else {
            ItemDownloadState::Complete
        };
        inner.publish(Some(server_id), DownloadEvent::ItemStateChanged { item_id: item_id.to_string(), state });
    }

    /// Flips the cancel flag for every track currently in flight for this item. Cooperative, not a
    /// hard abort — each track's own loop only checks it between chunks (see
    /// `abs_core::download_tracks::download_track`'s doc comment), so a cancellation always lands
    /// between well-defined units of work. Tracks that have already reached `Complete` are
    /// untouched, matching the ui-spec's "canceling leaves completed chapters in place."
    pub fn cancel_item(&self, server_id: &str, item_id: &str) {
        let inner = self.inner.borrow();
        if let Some(batch) = inner.batches.get(&(server_id.to_string(), item_id.to_string())) {
            for flag in &batch.cancel_flags {
                flag.set(true);
            }
        }
    }

    /// Cancels anything in flight for this item, then deletes every downloaded track file and row.
    /// Note: canceling is cooperative (see `cancel_item`), so a track's in-flight write and this
    /// deletion are not strictly ordered — a track that hasn't yet observed the cancellation when
    /// the delete runs could still write a few more bytes to a file this call is removing, or
    /// resurrect a row via its own next progress checkpoint. Accepted as a known race for this
    /// pass (clearing while actively downloading is an edge case, not the common path); a future
    /// pass could close it by having `clear_item` await the canceled tracks' own completion first.
    ///
    /// `on_done` gets the outcome, so the caller can confirm success or report a failure instead
    /// of toasting before the delete has actually happened.
    pub fn clear_item(&self, server_id: &str, item_id: &str, on_done: impl FnOnce(abs_core::Result<()>) + 'static) {
        self.cancel_item(server_id, item_id);
        let inner_rc = self.inner.clone();
        let server_id = server_id.to_string();
        let item_id = item_id.to_string();
        glib::spawn_future_local(async move {
            let pool = inner_rc.borrow().pool.clone();
            if let Err(err) = abs_core::download_tracks::clear_item_downloads(&pool, &server_id, &item_id).await {
                on_done(Err(err));
                return;
            }
            inner_rc.borrow().publish(Some(&server_id), DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Idle });
            on_done(Ok(()));
        });
    }

    /// Cancels every download in flight for `server_id` — for Remove Server, which deletes the
    /// server's files itself and mustn't race a download still writing into them.
    pub fn cancel_server(&self, server_id: &str) {
        let inner = self.inner.borrow();
        for (key, batch) in inner.batches.iter() {
            if key.0 == server_id {
                for flag in &batch.cancel_flags {
                    flag.set(true);
                }
            }
        }
    }

    /// Cancels anything in flight for this server, then deletes every downloaded track file and
    /// row across every item — the server-scoped mirror of `clear_item`, with the same known
    /// cooperative-cancel race documented there. `on_done` gets the outcome, same as `clear_item`.
    pub fn clear_all(&self, server_id: &str, on_done: impl FnOnce(abs_core::Result<()>) + 'static) {
        {
            let inner = self.inner.borrow();
            for (key, batch) in inner.batches.iter() {
                if key.0 == server_id {
                    for flag in &batch.cancel_flags {
                        flag.set(true);
                    }
                }
            }
        }
        let inner_rc = self.inner.clone();
        let server_id = server_id.to_string();
        glib::spawn_future_local(async move {
            let pool = inner_rc.borrow().pool.clone();
            match abs_core::download_tracks::clear_all_downloads(&pool, &server_id).await {
                Ok(item_ids) => {
                    {
                        let inner = inner_rc.borrow();
                        for item_id in item_ids {
                            inner.publish(Some(&server_id), DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Idle });
                        }
                    }
                    on_done(Ok(()));
                }
                Err(err) => on_done(Err(err)),
            }
        });
    }
}

/// Keeps a screen's "which books are downloaded" set current while it's open: whenever an item's
/// download finishes, stops, fails or is cleared, the set is read again
/// (`abs_core::download_tracks::downloaded_item_ids`, the same query the screens load with) and,
/// if it changed, handed to `on_changed`. Before, Home's and Library's badges only learned of a
/// download at the next sync, offline-mode toggle or restart.
///
/// `alive` is the screen's root: once it's gone the listener drops itself. Reads never overlap — a
/// change arriving during one is picked up by one more read right after.
pub(crate) fn follow_downloaded_items(
    download_manager: &DownloadManager,
    pool: SqlitePool,
    server_id: String,
    alive: glib::WeakRef<gtk4::Widget>,
    on_changed: impl Fn(HashSet<String>) + 'static,
) {
    struct Follow {
        pool: SqlitePool,
        server_id: String,
        reading: Cell<bool>,
        again: Cell<bool>,
        on_changed: Box<dyn Fn(HashSet<String>)>,
    }
    fn refresh(follow: Rc<Follow>) {
        if follow.reading.replace(true) {
            follow.again.set(true);
            return;
        }
        glib::spawn_future_local(async move {
            loop {
                follow.again.set(false);
                match abs_core::download_tracks::downloaded_item_ids(&follow.pool, &follow.server_id).await {
                    Ok(ids) => (follow.on_changed)(ids.into_iter().collect()),
                    Err(err) => tracing::warn!(%err, "couldn't re-read which books are downloaded; badges may be stale until the next sync"),
                }
                if !follow.again.get() {
                    break;
                }
            }
            follow.reading.set(false);
        });
    }
    let follow = Rc::new(Follow { pool, server_id, reading: Cell::new(false), again: Cell::new(false), on_changed: Box::new(on_changed) });
    download_manager.add_scoped_listener(move |event| {
        if alive.upgrade().is_none() {
            return false;
        }
        if let DownloadEvent::ItemStateChanged { state, .. } = event {
            if !matches!(state, ItemDownloadState::Downloading) {
                refresh(follow.clone());
            }
        }
        true
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::{pool, pump_until, test_paths};
    use abs_storage::models::DownloadStatus;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `abs_player::network_watch::FakeNetworkMonitor` is `#[cfg(test)]`-only *inside abs-player*,
    /// so it isn't compiled in when abs-player is built as an ordinary dependency of this crate's
    /// own test binary (same cross-crate `cfg(test)` visibility gap `main_window.rs`'s own tests
    /// document for `call_watch::FakeCallWatcher`) — a small local fake against the public
    /// `NetworkMonitor` trait sidesteps that instead.
    struct FakeNetworkMonitor {
        metered: Option<bool>,
    }

    impl NetworkMonitor for FakeNetworkMonitor {
        fn is_metered(&self) -> Option<bool> {
            self.metered
        }
    }

    async fn session_for(pool: &SqlitePool, server_url: &str) -> (Session, abs_storage::models::Server) {
        let server_id = abs_storage::repo::servers::add(pool, server_url).await.unwrap();
        let account_id = abs_storage::repo::accounts::add(pool, &server_id, "jane", "token123", None).await.unwrap();
        let server = abs_storage::repo::servers::get(pool, &server_id).await.unwrap();
        let account = abs_storage::repo::accounts::get(pool, &account_id).await.unwrap();
        (Session::new(pool.clone(), &server, &account), server)
    }

    async fn insert_synced_item(pool: &SqlitePool, server_id: &str, item_id: &str) {
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
    }

    /// A two-file item, one chapter per file, so `DownloadScope::CurrentChapter` maps onto exactly
    /// one track — the simplest possible case that still exercises chapter->track resolution.
    pub(crate) async fn mock_two_track_item(mock_server: &MockServer, item_id: &str) {
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
        for ino in ["1", "2"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/items/{item_id}/file/{ino}")))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "audio/mpeg").insert_header("Content-Length", "5").set_body_bytes(b"hello".to_vec()))
                .mount(mock_server)
                .await;
        }
    }

    /// A three-file item, one chapter per file, with the server's per-file `metadata.size`
    /// populated (1/2/4 MB) — the smallest case where the download sheet's size-estimate
    /// subtitles can be watched change as the "Next chapters" stepper steps.
    pub(crate) async fn mock_three_track_item(mock_server: &MockServer, item_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [
                        { "ino": "1", "duration": 5.0, "metadata": { "size": 1_000_000 } },
                        { "ino": "2", "duration": 5.0, "metadata": { "size": 2_000_000 } },
                        { "ino": "3", "duration": 5.0, "metadata": { "size": 4_000_000 } },
                    ],
                    "chapters": [
                        { "id": 0, "start": 0.0, "end": 5.0, "title": "Chapter 1" },
                        { "id": 1, "start": 5.0, "end": 10.0, "title": "Chapter 2" },
                        { "id": 2, "start": 10.0, "end": 15.0, "title": "Chapter 3" },
                    ],
                }
            })))
            .mount(mock_server)
            .await;
        for ino in ["1", "2", "3"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/items/{item_id}/file/{ino}")))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "audio/mpeg").insert_header("Content-Length", "5").set_body_bytes(b"hello".to_vec()))
                .mount(mock_server)
                .await;
        }
    }

    /// Like `mock_two_track_item`, but with no chapter data at all — the degenerate case
    /// `start_download` falls back to "download every track" for, since there's no finer-grained
    /// scope to resolve against.
    async fn mock_two_track_item_without_chapters(mock_server: &MockServer, item_id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [{ "ino": "1", "duration": 5.0 }, { "ino": "2", "duration": 5.0 }] }
            })))
            .mount(mock_server)
            .await;
        for ino in ["1", "2"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/items/{item_id}/file/{ino}")))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "audio/mpeg").insert_header("Content-Length", "5").set_body_bytes(b"hello".to_vec()))
                .mount(mock_server)
                .await;
        }
    }

    /// A single-track item whose file response is deliberately slow, giving `cancel_item` tests a
    /// real window to cancel inside before the transfer would otherwise complete.
    async fn mock_slow_single_track_item(mock_server: &MockServer, item_id: &str, delay: Duration) {
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [{ "ino": "1", "duration": 10.0 }] }
            })))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}/file/1")))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "5").set_delay(delay).set_body_bytes(b"hello".to_vec()))
            .mount(mock_server)
            .await;
    }

    pub(crate) fn run_start_download_fetches_only_the_needed_track_and_reaches_complete(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(pool());
        let (session, server) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1"));

        let manager = DownloadManager::new(pool.clone(), test_paths(), Box::new(FakeNetworkMonitor { metered: None }), false);

        let events: Rc<RefCell<Vec<DownloadEvent>>> = Rc::new(RefCell::new(Vec::new()));
        manager.add_listener({
            let events = events.clone();
            move |event| events.borrow_mut().push(event.clone())
        });

        manager.start_download(session, "item-1".to_string(), DownloadScope::CurrentChapter, 0);

        pump_until(
            || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Complete, .. })),
            Duration::from_secs(10),
        );
        assert!(
            events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Complete, .. })),
            "the download should have reached Complete"
        );

        // Only chapter 0's track ("1") should ever have been requested — chapter 1's track ("2")
        // is out of scope for `CurrentChapter` and must not be fetched.
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(requests.iter().any(|r| r.url.path() == "/api/items/item-1/file/1"));
        assert!(!requests.iter().any(|r| r.url.path() == "/api/items/item-1/file/2"), "a track outside the requested scope must not be fetched");

        let row = runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Complete);
    }

    pub(crate) fn run_cancel_item_stops_the_track_from_reaching_complete(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_slow_single_track_item(&mock_server, "item-1", Duration::from_secs(2)));

        let pool = runtime.block_on(pool());
        let (session, server) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1"));

        let manager = DownloadManager::new(pool.clone(), test_paths(), Box::new(FakeNetworkMonitor { metered: None }), false);

        let events: Rc<RefCell<Vec<DownloadEvent>>> = Rc::new(RefCell::new(Vec::new()));
        manager.add_listener({
            let events = events.clone();
            move |event| events.borrow_mut().push(event.clone())
        });

        manager.start_download(session, "item-1".to_string(), DownloadScope::EntireBook, 0);

        // Give the manager a moment to actually issue the request before canceling — waiting for
        // `Downloading` (published synchronously once the batch is set up) rather than a fixed
        // sleep keeps this deterministic.
        pump_until(
            || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Downloading, .. })),
            Duration::from_secs(5),
        );
        manager.cancel_item(&server.id, "item-1");

        pump_until(
            || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Stopped, .. })),
            Duration::from_secs(10),
        );
        assert!(
            events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Stopped, .. })),
            "a fully-canceled batch with nothing completed should settle on Stopped — the user's stop, not a failure"
        );

        let row = runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap();
        assert!(row.is_none() || row.unwrap().status != DownloadStatus::Complete, "canceling must not let the track reach Complete");
    }

    pub(crate) fn run_wifi_only_blocks_a_download_on_a_metered_connection(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(pool());
        let (session, server) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1"));

        let manager = DownloadManager::new(pool.clone(), test_paths(), Box::new(FakeNetworkMonitor { metered: Some(true) }), true);

        let events: Rc<RefCell<Vec<DownloadEvent>>> = Rc::new(RefCell::new(Vec::new()));
        manager.add_listener({
            let events = events.clone();
            move |event| events.borrow_mut().push(event.clone())
        });

        manager.start_download(session, "item-1".to_string(), DownloadScope::CurrentChapter, 0);

        pump_until(
            || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Failed(_), .. })),
            Duration::from_secs(5),
        );
        assert!(
            events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Failed(_), .. })),
            "a metered connection with wifi_only set should fail the download rather than fetch it"
        );

        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(!requests.iter().any(|r| r.url.path().starts_with("/api/items/item-1/file/")), "a metered connection with wifi_only set must never fetch the file");
    }

    pub(crate) fn run_clear_item_deletes_files_and_publishes_idle(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(pool());
        let (session, server) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1"));

        let manager = DownloadManager::new(pool.clone(), test_paths(), Box::new(FakeNetworkMonitor { metered: None }), false);
        manager.start_download(session, "item-1".to_string(), DownloadScope::CurrentChapter, 0);
        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|r| r.status == DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );

        let events: Rc<RefCell<Vec<DownloadEvent>>> = Rc::new(RefCell::new(Vec::new()));
        manager.add_listener({
            let events = events.clone();
            move |event| events.borrow_mut().push(event.clone())
        });

        manager.clear_item(&server.id, "item-1", |result| result.unwrap());

        pump_until(
            || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { state: ItemDownloadState::Idle, .. })),
            Duration::from_secs(10),
        );

        assert!(runtime.block_on(abs_storage::repo::download_tracks::list_for_item(&pool, &server.id, "item-1")).unwrap().is_empty());
    }

    /// An item with no chapter markers at all has nothing for `resolve_scope`/
    /// `tracks_needed_for_chapters` to select from — every scope must degenerate to "the whole
    /// book" rather than downloading nothing.
    pub(crate) fn run_start_download_with_no_chapters_fetches_every_track(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item_without_chapters(&mock_server, "item-1"));

        let pool = runtime.block_on(pool());
        let (session, server) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1"));

        let manager = DownloadManager::new(pool.clone(), test_paths(), Box::new(FakeNetworkMonitor { metered: None }), false);
        manager.start_download(session, "item-1".to_string(), DownloadScope::CurrentChapter, 0);

        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::list_for_item(&pool, &server.id, "item-1")).unwrap().iter().filter(|d| d.status == DownloadStatus::Complete).count() == 2,
            Duration::from_secs(10),
        );

        let rows = runtime.block_on(abs_storage::repo::download_tracks::list_for_item(&pool, &server.id, "item-1")).unwrap();
        assert_eq!(rows.iter().filter(|d| d.status == DownloadStatus::Complete).count(), 2, "with no chapters, every track should be fetched regardless of scope");
    }

    /// Toggling `wifi_only` mid-batch only affects downloads started *after* the change — a track
    /// whose metered check already passed keeps running.
    pub(crate) fn run_set_wifi_only_only_affects_downloads_started_afterwards(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, "item-1"));
        runtime.block_on(mock_two_track_item(&mock_server, "item-2"));

        let pool = runtime.block_on(pool());
        let (session, server) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2"));

        let manager = DownloadManager::new(pool.clone(), test_paths(), Box::new(FakeNetworkMonitor { metered: Some(true) }), false);

        let events: Rc<RefCell<Vec<DownloadEvent>>> = Rc::new(RefCell::new(Vec::new()));
        manager.add_listener({
            let events = events.clone();
            move |event| events.borrow_mut().push(event.clone())
        });

        // Started while wifi_only is still false — a metered connection must not block it.
        manager.start_download(session.clone(), "item-1".to_string(), DownloadScope::CurrentChapter, 0);
        pump_until(
            || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Complete, .. } if item_id == "item-1"))
                || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Failed(_), .. } if item_id == "item-1")),
            Duration::from_secs(10),
        );
        assert!(
            events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Complete, .. } if item_id == "item-1")),
            "a download already in flight when wifi_only was false must not be blocked by a later toggle"
        );

        manager.set_wifi_only(true);

        // Started after the toggle, still on a metered connection — this one must be blocked.
        manager.start_download(session, "item-2".to_string(), DownloadScope::CurrentChapter, 0);
        pump_until(
            || events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Failed(_), .. } if item_id == "item-2")),
            Duration::from_secs(5),
        );
        assert!(
            events.borrow().iter().any(|e| matches!(e, DownloadEvent::ItemStateChanged { item_id, state: ItemDownloadState::Failed(_), .. } if item_id == "item-2")),
            "a download started after wifi_only was set true, on a metered connection, must be blocked"
        );
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. With offline mode on, a download isn't
    /// started (and says why) instead of reaching for the server.
    pub(crate) fn run_offline_mode_starts_no_download(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        let pool = runtime.block_on(crate::test_support::pool());
        let (session, _server) = runtime.block_on(session_for(&pool, &mock_server.uri()));
        session.set_offline(true);
        let manager = DownloadManager::new(pool, crate::test_support::test_paths(), Box::new(abs_player::network_watch::UnknownNetworkMonitor), false);
        let states: Rc<RefCell<Vec<ItemDownloadState>>> = Rc::new(RefCell::new(Vec::new()));
        manager.add_listener({
            let states = states.clone();
            move |event| {
                if let DownloadEvent::ItemStateChanged { state, .. } = event {
                    states.borrow_mut().push(state.clone());
                }
            }
        });
        manager.start_download(session, "item-1".to_string(), DownloadScope::EntireBook, 0);
        crate::test_support::pump_until(|| !states.borrow().is_empty(), std::time::Duration::from_secs(5));
        assert!(matches!(states.borrow().first(), Some(ItemDownloadState::Failed(reason)) if reason.contains("offline")), "{:?}", states.borrow());
        crate::test_support::pump_until(|| false, std::time::Duration::from_millis(300));
        assert!(runtime.block_on(mock_server.received_requests()).unwrap().is_empty(), "offline mode: nothing may reach the server");
    }
}
