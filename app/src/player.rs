//! Bridges `abs-player`'s poll-based `AudioBackend` to GTK widgets: a mini-player bar (built here,
//! always present in `main_window` once something has played) and, optionally, a full player
//! screen's widgets (`screens::player`). This module — not `abs_core::playback::PlaybackState` —
//! is what actually drives real playback.
//!
//! `PlaybackState` is a simulated tick-based clock (`position += elapsed * speed`) with no method
//! to feed it an authoritative position from a real backend and no reference to `abs_player`
//! anywhere in `abs-core` (by design — `abs-core` has zero GStreamer dependency). Wiring a real
//! GStreamer pipeline's displayed position to that simulated clock would let the two drift apart,
//! which is a correctness bug, not an architecture shortcut — so this controller polls
//! `AudioBackend` directly instead, and `PlaybackState` stays unused. Don't "helpfully" reconnect
//! them without first giving `PlaybackState` a real way to reconcile with actual backend state.
//!
//! Never imports `abs_api`: the only network call this makes is
//! `abs_core::streaming::resolve_stream_target`, which builds its own authenticated
//! `abs_api::Client` internally — same boundary every other screen already respects.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use abs_core::streaming::locate_track;
use abs_storage::AppPaths;

use crate::i18n::tr;
use crate::widgets::cover_image::CoverImage;

/// The URL/URI to actually load for a track: a local `file://` path when it's been verifiably
/// downloaded (see `abs_core::download_tracks::local_track_path` — never a stale/corrupt row), a
/// streaming URL otherwise. `gio::File::for_path(..).uri()` is the same correctly-percent-encoded
/// pattern this file already uses for MPRIS album-art URLs, not a raw `format!("file://{}", ..)`.
/// The access token is only fetched in the streaming branch — pointless (and, if genuinely
/// offline, noisy) to refresh a token for a track that's about to play from disk anyway.
/// Where a start should resume from, given the saved progress and the book's duration as just
/// resolved. A finished book starts over. Anything else resumes at the saved position, even one
/// at or past the reported end: durations can come back missing or short (and are corrected
/// upward during playback), and dropping the position there would restart at 0 and save that
/// over it seconds later. Such a position is clamped just inside the book instead.
fn resume_position(saved_seconds: f64, is_finished: bool, duration_seconds: f64) -> Option<f64> {
    if is_finished || saved_seconds <= 0.0 {
        return None;
    }
    if duration_seconds > 0.0 && saved_seconds >= duration_seconds {
        return Some((duration_seconds - 1.0).max(0.0)).filter(|s| *s > 0.0);
    }
    Some(saved_seconds)
}

async fn resolve_playable_url(
    pool: &SqlitePool,
    connection: &abs_core::connection::ConnectionTarget,
    server_id: &str,
    item_id: &str,
    ino: &str,
    session: &abs_core::auth::Session,
) -> (String, bool) {
    match abs_core::download_tracks::track_source(pool, server_id, item_id, ino).await {
        abs_core::download_tracks::TrackSource::Local(path) => return (gio::File::for_path(&path).uri().to_string(), true),
        // Never downloaded is the ordinary streaming case; anything else means a download exists
        // that couldn't be used, which is worth knowing when a "downloaded" book streams.
        abs_core::download_tracks::TrackSource::Streamed(abs_core::download_tracks::StreamReason::NotDownloaded) => {}
        abs_core::download_tracks::TrackSource::Streamed(reason) => {
            tracing::warn!(item_id, ino, %reason, "streaming a track that has a download, which can't be used");
        }
    }
    (connection.track_url(item_id, ino, &session.access_token().await), false)
}

/// For a start: the cached track list and chapters, if the track playback would start in is
/// downloaded and verified on disk — i.e. the book can start without the server. `None` when
/// there is no cached metadata or the start track would have to be streamed.
async fn start_target_from_files(
    pool: &SqlitePool,
    session: &abs_core::auth::Session,
    item_id: &str,
    start_chapter: Option<usize>,
    connection: &abs_core::connection::ConnectionTarget,
    access_token: &str,
) -> Option<abs_core::streaming::StreamTarget> {
    let target = abs_core::streaming::offline_stream_target(pool, session.server_id(), item_id, connection, access_token).await.ok()?;
    let chapter_start = start_chapter.and_then(|index| target.chapters.get(index)).map(|c| c.start_seconds.max(0.0));
    let saved = abs_storage::repo::progress::get(pool, session.account_id(), session.server_id(), item_id).await.ok().flatten();
    let resume_at = match chapter_start {
        Some(at) => Some(at).filter(|at| *at > 0.0),
        None => saved.and_then(|p| resume_position(p.current_time_seconds, p.is_finished, target.duration_seconds)),
    };
    let (start_track, _) = locate_track(&target.tracks, resume_at.unwrap_or(0.0));
    let ino = &target.tracks.get(start_track)?.ino;
    match abs_core::download_tracks::track_source(pool, session.server_id(), item_id, ino).await {
        abs_core::download_tracks::TrackSource::Local(_) => Some(target),
        abs_core::download_tracks::TrackSource::Streamed(reason) => {
            tracing::info!(item_id, track = start_track, %reason, "the track to start in isn't on the device; the server is needed to start");
            None
        }
    }
}

/// The app is the composition root between `abs-core` (which resolves a server's connection
/// settings) and `abs-player` (which applies them to GStreamer) — this is the mapping between
/// the two, so neither crate depends on the other.
fn playback_properties(connection: &abs_core::connection::ConnectionTarget) -> abs_player::ConnectionProperties {
    abs_player::ConnectionProperties {
        extra_headers: connection.extra_headers().to_vec(),
        user_agent: Some(connection.user_agent().to_string()),
        ssl_strict: !connection.disable_ssl_verify(),
    }
}

const TICK_INTERVAL: Duration = Duration::from_millis(250);
/// How often the *local* progress row is refreshed while playing, from the periodic tick — cheap
/// (a WAL-mode SQLite upsert, no network) and worth keeping frequent so "Continue Listening"
/// never looks far behind.
const LOCAL_PROGRESS_WRITE_INTERVAL: Duration = Duration::from_secs(5);
/// How often that same periodic tick additionally pushes progress to the *server* while playing.
/// Deliberately much coarser than the local write: syncing costs a real HTTP round trip (a radio
/// wakeup on a phone, even with `Session::api_client`'s client cache removing the handshake), and
/// every meaningful state change (pause, end-of-book, a sleep timer firing, switching items) has
/// its own explicit `write_progress` call that syncs immediately regardless of this interval — see
/// `write_progress_at`'s `force_server_sync`. 15s matches the interval Audiobookshelf's official
/// clients use for the same "still playing" push.
const SERVER_PROGRESS_SYNC_INTERVAL: Duration = Duration::from_secs(15);
/// How long an issued seek may report a position away from its target before
/// `Inner::observe_position` asks again.
const SEEK_REISSUE_AFTER: Duration = Duration::from_secs(2);
const MAX_SEEK_REISSUES: u8 = 3;
/// A resume after a pause at least this long checks the server for newer progress first — long
/// enough to have plausibly listened on another device meanwhile.
const RESUME_RECONCILE_AFTER: Duration = Duration::from_secs(60);
/// How far the server's position must be from this device's before a resume adopts it.
const ADOPT_SERVER_POSITION_MIN_DELTA_SECONDS: f64 = 5.0;
/// A seek while paused is saved once the listener has stopped moving the position for this long
/// — scrubbing emits many seeks, and each doesn't need its own write and push.
const PAUSED_SEEK_WRITE_DELAY: Duration = Duration::from_secs(2);
/// See `Inner::premature_end_of_stream`.
const PREMATURE_EOS_MIN_GAP_SECONDS: f64 = 60.0;
/// How long quitting may wait for the final progress push.
const SHUTDOWN_PUSH_TIMEOUT: Duration = Duration::from_secs(3);
/// How `pause()`'s confirmation poll (`Inner::confirm_pause_landed`) is paced: checked this
/// often, for up to this many attempts, before treating a pause that never actually reached
/// GStreamer's `Paused` state as stuck. `AudioBackend::pause()` returning `Ok` only means the
/// request was accepted, not that it landed — see its doc comment.
const PAUSE_CONFIRM_INTERVAL: Duration = Duration::from_millis(300);
const PAUSE_CONFIRM_ATTEMPTS: u32 = 10;
/// How soon after an unplug pause an MPRIS `PlayPause` is treated as the spurious headset-button
/// press a TRRS unplug generates rather than a real one — see
/// `PlayerController::external_play_pause`. Observed on the Librem 5: the spurious one ~2ms
/// after the pause, a deliberate one ~3s after.
const SPURIOUS_UNPLUG_TOGGLE_WINDOW: Duration = Duration::from_millis(1500);

/// How many 100 ms polls a track load or a start waits for the pipeline to preroll before a
/// seek is attempted anyway (and then trusted only once `observe_position` sees it land): 15 s,
/// matching the HTTP timeout. 5 s gave up on slow mobile connections that were still connecting.
const PREROLL_WAIT_ATTEMPTS: u32 = 150;

/// How long a start waits for the server when the book can start from the files on the device
/// (the cached track list is there and the track to start in is downloaded). A dead connection
/// used to cost the full HTTP timeout (15 s) before the downloaded files were used.
const LOCAL_START_SERVER_WAIT: Duration = Duration::from_secs(3);

/// How long a headphone replug has to hold before it resumes playback: a jack that bounces
/// (plugged, unplugged, plugged again within a few hundred ms) is not one reconnection. The
/// unplug side is not delayed — pausing at once is the point of it.
const REPLUG_SETTLE: Duration = Duration::from_millis(300);
/// A stream error while playing reloads the file once by itself (a fresh URL, token and
/// connection) — a dropped connection or an expired token is the common cause, and a reload is
/// exactly what Retry would do. A second error within this window stops with the error instead.
const AUTO_RECOVER_WINDOW: Duration = Duration::from_secs(60);
/// After a pause this long, a streamed book resumes with a fresh load rather than asking the
/// paused pipeline to continue: the phone may have slept or changed networks meanwhile, and the
/// pipeline's old HTTP connection then only fails after its own retries and timeouts — a long
/// stretch of silence while the player says it's playing.
const STALE_CONNECTION_AFTER: Duration = Duration::from_secs(300);
/// After an automatic reload, how long the stream may take to answer before that is logged.
const STREAM_SLOW_TO_ANSWER_AFTER: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct PlayRequest {
    pub item_id: String,
    pub title: String,
    pub author: Option<String>,
}

#[derive(Clone)]
pub struct PlayerSnapshot {
    pub title: String,
    pub author: Option<String>,
    /// Book-level position — the current file's offset within the book plus the pipeline's
    /// position inside that file. Progress and chapters are defined book-level, so every
    /// consumer of a snapshot (scrubber, mini bar, MPRIS) speaks the same units.
    pub position_seconds: f64,
    pub duration_seconds: f64,
    pub is_playing: bool,
    pub speed: f64,
    pub sleep_timer_active: bool,
    pub cover_path: Option<std::path::PathBuf>,
    /// The most recent playback failure, if any — `None` means the last load/play attempt (or the
    /// current one, if nothing has failed) is fine. Set by every failure path in `Inner` (a bus
    /// error mid-playback, a failed track load, a failed synchronous `play`/`pause`), cleared
    /// whenever the corresponding action next succeeds. `screens::player`'s error banner and the
    /// mini bar's warning glyph are this field's only two readers.
    pub last_error: Option<abs_player::PlaybackError>,
    /// Set from the moment `start()` is asked for a book until that book has resolved and is
    /// loaded — position and duration are 0 meanwhile, and `is_playing` is `false` because
    /// nothing is audible yet; `will_play_when_loaded` says whether it will start playing once
    /// loaded (the user, an unplug or a call can pause it before then).
    pub is_loading: bool,
    pub will_play_when_loaded: bool,
    /// Whether the loaded book has chapter data, so the previous/next chapter buttons have
    /// anywhere to go. `false` while a start is still resolving.
    pub has_chapters: bool,
    /// The chapter the position falls in, for the full player's chapter line. `None` while a
    /// start is still resolving or when the book has no chapters.
    pub chapter: Option<CurrentChapter>,
    /// Whether the track playing now is read from a file on the device rather than streamed.
    /// Meaningless (`false`) while a start is still resolving.
    pub source_is_local: bool,
}

/// Which chapter is playing, as the full player shows it: "<title> · <number> of <count>".
#[derive(Clone, Debug, PartialEq)]
pub struct CurrentChapter {
    pub title: String,
    /// 1-based.
    pub number: usize,
    pub count: usize,
    /// Book-level bounds, which the full player's scrubber spans.
    pub start_seconds: f64,
    pub end_seconds: f64,
}

impl PlayerSnapshot {
    /// Whether a play/pause button should currently offer "pause": playing, or loading with the
    /// intent to play once loaded.
    pub fn shows_pause_button(&self) -> bool {
        self.is_playing || (self.is_loading && self.will_play_when_loaded)
    }
}

/// A book `start()` has been asked for that hasn't resolved and loaded yet. Held apart from
/// `NowPlaying` so nothing can act on a book that isn't loaded: every transport action already
/// does nothing without a `NowPlaying`, and only play/pause need to know about this (they change
/// `wants_play`). The previous book is stopped and dropped the moment a start is asked for — it
/// used to stay loaded through the whole resolve, and a rewind or a track change in that window
/// loaded the old book's audio after the new one's, playing it under the new book's title.
struct PendingStart {
    /// `Inner::session_generation` of the `start()` this is for.
    session_generation: u64,
    item_id: String,
    server_id: String,
    account_id: String,
    title: String,
    author: Option<String>,
    cover_path: Option<std::path::PathBuf>,
    wants_play: bool,
    /// The chapter to start at instead of the saved position, if one was tapped.
    start_chapter: Option<usize>,
    /// What Item Detail's "Reset progress" / "Mark as finished" asked for while this book was
    /// still starting. Applied once it is loaded (see `PlayerController::reset_progress`).
    intent: Option<StartIntent>,
    /// What the book is being started with — what a download button on a Player opened while it
    /// loads needs.
    session: abs_core::auth::Session,
}

/// A progress action asked for during a start; see [`PendingStart::intent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartIntent {
    Reset,
    MarkFinished,
}

/// A chapter, as needed by the chapters sheet — kept in-memory on `NowPlaying` rather than pushed
/// through `PlayerSnapshot` on every 250ms tick, since chapters don't change during playback.
#[derive(Clone)]
pub struct ChapterInfo {
    pub title: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
}

/// A sleep-timer deadline, checked once per `tick()` rather than driven by a second timer source
/// — a wall-clock deadline (15/30/45 min) and an end-of-chapter deadline (a stream position) are
/// otherwise different units entirely, but both reduce to "has `now` (real time, or playback
/// position) reached this yet?".
#[derive(Clone, Copy, PartialEq)]
enum SleepTimerDeadline {
    WallClock(Instant),
    Position(f64),
}

#[derive(Clone, Copy, PartialEq, Default)]
enum SleepTimerState {
    #[default]
    Off,
    Armed(SleepTimerDeadline),
}

struct NowPlaying {
    item_id: String,
    server_id: String,
    account_id: String,
    /// The live token source — asked per use (progress writes, track loads) rather than a token
    /// captured once, which dies within hours on servers v2.26.0+ while a book keeps playing.
    /// The connection (settings + resolved base URL) is asked through it at the same times.
    session: abs_core::auth::Session,
    title: String,
    author: Option<String>,
    /// Book-level duration (the sum of every track's duration), as `StreamTarget` reports it.
    duration_seconds: f64,
    /// One entry per audio file of the item, in book order — an item split across multiple files
    /// is played by advancing through these as each one ends.
    tracks: Vec<abs_core::streaming::StreamTrack>,
    /// Which entry of `tracks` the backend currently holds loaded.
    current_track: usize,
    /// Whether `current_track`'s currently-loaded source is a local (downloaded) file rather
    /// than an HTTP stream — set from `resolve_playable_url`'s own verdict at every load. Used
    /// to decide, at the next seek or stream error, whether a since-completed download can now
    /// take over from a stream that started before it finished (see `seek_to_seconds` and
    /// `tick`'s `Error` handling) — never mid-playback, only at those natural discontinuities.
    current_source_is_local: bool,
    is_playing: bool,
    chapters: Vec<ChapterInfo>,
    speed: f64,
    sleep_timer: SleepTimerState,
    cover_path: Option<std::path::PathBuf>,
    /// See [`PlayerSnapshot::last_error`]'s doc comment — same field, mirrored here since
    /// `NowPlaying` is what `snapshot()` actually reads from.
    last_error: Option<abs_player::PlaybackError>,
    /// The last within-track position the backend actually reported, or the target of the most
    /// recent seek — what `book_position` falls back to while `backend.position()` reports
    /// `None`. A `FLUSH` seek on a network-streamed file has no segment (and so no position)
    /// until the new HTTP range response actually arrives, which on a stalled/flaky connection
    /// can be seconds to never; without this, `book_position` would read as "start of the
    /// current track" for that whole window, and every progress write and repeated skip in it
    /// would act on that bogus position (see this plan's Librem 5 field report).
    last_known_within_track: f64,
    /// Set the instant a same-track seek is *requested*, cleared once `observe_position`
    /// confirms the backend has actually landed near `last_known_within_track` (see its own
    /// check). While set, `book_position` uses `last_known_within_track` unconditionally,
    /// ignoring `backend.position()` even when it returns `Some` — necessary because a query
    /// right after requesting a seek, but before the backend has actually issued it (e.g. while
    /// `seek_to_seconds`'s own async "did a download just finish" check is still running — see
    /// section E), reports the *pipeline's pre-seek* position: real, `Some`, and just as wrong
    /// as the `None` a stalled `FLUSH` seek reports once the seek has actually been issued to a
    /// network-streamed source. One flag covers both windows.
    seek_target_pending: bool,
    /// Set when a bus `Error` (or a failed `load()`/state change) left the backend's pipeline
    /// reset to a released, unloaded state (see `AudioBackend::reset`) rather than merely
    /// paused. While set, `play()` does not ask the dead pipeline to resume — it reloads the
    /// current track from `last_known_within_track` instead (the same path a cross-track seek
    /// already uses), and `pause()` skips `backend.pause()` (nothing loaded to pause).
    needs_reload: bool,
    /// When the seek behind `seek_target_pending` was actually handed to the backend — `None`
    /// while it hasn't been yet (a cross-track reload or the "did a download just finish" check
    /// is still in flight). Only an issued seek is ever re-issued by `observe_position`.
    seek_issued_at: Option<Instant>,
    /// How many times `observe_position` has re-issued the pending seek. A seek asked for before
    /// the pipeline could take it silently no-ops; without a re-issue the book would play on from
    /// the start of the track, and that position would be saved over the real one.
    seek_retries: u8,
    /// What to run `start()` with again when this `NowPlaying` came from a start that failed
    /// before any track could be resolved (`tracks` is empty). Retry then re-runs the whole start
    /// instead of resuming whatever the backend still held from the previous book.
    retry_request: Option<(PlayRequest, f64)>,
    /// Where the last end-of-stream that looked premature happened, as `(track, within)` — see
    /// `Inner::premature_end_of_stream`.
    premature_eos_at: Option<(usize, f64)>,
    /// `Some(load generation)` while `spawn_load_track` is bringing `current_track`'s file into
    /// the backend. Meanwhile the backend holds nothing usable: a seek within that track only
    /// moves `last_known_within_track` (the load seeks there once ready), play/pause only change
    /// `is_playing` (the load applies it), and a speed change only changes `speed`.
    loading_track: Option<u64>,
    /// The book has been finished: it reached its end, or was marked finished. Every progress
    /// write then records it finished (at its full duration) rather than unfinished at wherever
    /// the pipeline stopped, and the next play starts it over. Cleared by any seek.
    ended: bool,
    /// `Some(speed the backend actually runs at)` while a speed picked during a pause on a
    /// stream hasn't been handed to the backend yet — applying it is a flushing seek, i.e. a
    /// range request to the server, and nothing is heard until Play anyway. `play()` applies it;
    /// if the backend refuses it, this is the speed to show again. `speed` is the picked one.
    speed_unapplied: Option<f64>,
    /// The seek target never landed after its re-issues and the file was reloaded once to get
    /// there (see `Inner::observe_position`). A second failure then stops with an error instead
    /// of reloading forever. Reset by a landed seek and by any seek the listener asks for.
    seek_reload_used: bool,
}

/// See `Inner::judge_end_of_stream`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum EndOfStreamVerdict {
    RealEnd,
    Unclear { within: f64, short_by: f64 },
    Premature,
}

/// What `seek_to_seconds` decided to do once its synchronous, borrow-scoped decision-making is
/// done — resolved just below it, outside that borrow, since one of the two cases needs an
/// async DB check first.
enum SeekPlan {
    /// Seeking into a different file than the one currently loaded — always a reload,
    /// regardless of source. `prepare_track_load` has already run; this is its load generation.
    CrossTrack { item_id: String, generation: u64 },
    /// Seeking within the currently-loaded, still-streamed track (the seek itself is already
    /// issued): worth checking whether a download finished since it started streaming, in which
    /// case the file is loaded instead.
    /// `load_generation` is the backend's at the time of the seek.
    MaybeLocalNow { item_id: String, server_id: String, track_index: usize, ino: Option<String>, load_generation: u64 },
}

/// How far before the end of the book a seek may land at most. Seeking to the very end (a
/// scrubber dragged all the way right, a long skip forward) used to reach end-of-stream at once
/// and mark the book finished; a book is finished by listening to its end, or by the explicit
/// "Mark as finished".
const SEEK_END_MARGIN_SECONDS: f64 = 1.0;

/// A playback speed the backend can be given: finite and within the range the speed picker
/// offers. Anything else (a hand-edited setting, a stored NaN) is brought into range, or is
/// `None` when there is nothing sensible to bring it to.
fn usable_speed(speed: f64) -> Option<f64> {
    speed.is_finite().then(|| speed.clamp(abs_core::playback::MIN_SPEED, abs_core::playback::MAX_SPEED))
}

/// A seek target clamped into the book, short of its very end by `SEEK_END_MARGIN_SECONDS`. A
/// book whose duration came back unknown (0) is only clamped below — clamping to its "end" would
/// send every seek to 0.
fn clamp_to_book(seconds: f64, duration_seconds: f64) -> f64 {
    if duration_seconds > SEEK_END_MARGIN_SECONDS {
        seconds.clamp(0.0, duration_seconds - SEEK_END_MARGIN_SECONDS)
    } else if duration_seconds > 0.0 {
        seconds.clamp(0.0, duration_seconds)
    } else {
        seconds.max(0.0)
    }
}

/// The end of the chapter `position` falls in, if any.
fn chapter_end_at(chapters: &[ChapterInfo], position: f64) -> Option<f64> {
    chapters.iter().find(|c| c.start_seconds <= position && position < c.end_seconds).map(|c| c.end_seconds)
}

/// How far into a chapter "previous chapter" still means "back to the previous one" — past this
/// it restarts the current chapter instead, the way a music player's previous button works.
const PREVIOUS_CHAPTER_RESTART_SECONDS: f64 = 3.0;

/// Index of the chapter `position` is in: the last one that has started by then. Past every
/// range that's the last chapter, and before the first one starts (a book whose first chapter
/// doesn't begin at 0) it's the first — not, as a "which range contains it" test would fall
/// through to, the last. `None` without chapters.
fn chapter_index_at(chapters: &[ChapterInfo], position: f64) -> Option<usize> {
    if chapters.is_empty() {
        return None;
    }
    Some(chapters.iter().rposition(|c| c.start_seconds <= position).unwrap_or(0))
}

/// Where "next chapter" goes from `position`: the start of the chapter after the current one.
/// `None` in the last chapter or without chapters.
fn next_chapter_start(chapters: &[ChapterInfo], position: f64) -> Option<f64> {
    let current = chapter_index_at(chapters, position)?;
    chapters.get(current + 1).map(|c| c.start_seconds.max(0.0))
}

/// Where "previous chapter" goes from `position`: the current chapter's start when more than
/// `PREVIOUS_CHAPTER_RESTART_SECONDS` into it (or in the first chapter), otherwise the previous
/// chapter's start. `None` without chapters.
fn previous_chapter_start(chapters: &[ChapterInfo], position: f64) -> Option<f64> {
    let current = chapter_index_at(chapters, position)?;
    let restart = position - chapters[current].start_seconds > PREVIOUS_CHAPTER_RESTART_SECONDS;
    let target = if restart || current == 0 { current } else { current - 1 };
    Some(chapters[target].start_seconds.max(0.0))
}

/// Returns whether to keep listening: a listener that returns `false` (its screen is gone) is
/// dropped after that call.
type SnapshotListener = Box<dyn Fn(&PlayerSnapshot) -> bool>;
/// The one optional slot the full player screen fills while it is open.
type FullUpdateListener = Box<dyn Fn(&PlayerSnapshot)>;

/// How one background push of playback progress to the server ended — see
/// `PlayerController::set_on_progress_sync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressSyncOutcome {
    Synced,
    /// The server said the session is no longer valid; retrying won't help until the user logs
    /// in again.
    SessionExpired,
    /// Anything else (unreachable server, server error). The next periodic sync retries.
    Failed,
}

type ProgressSyncListener = Rc<dyn Fn(ProgressSyncOutcome)>;

/// One progress write for `ProgressWriter`: the local row, and optionally a push to the server.
struct ProgressWrite {
    pool: SqlitePool,
    session: abs_core::auth::Session,
    account_id: String,
    server_id: String,
    item_id: String,
    position: f64,
    is_finished: bool,
    duration_seconds: f64,
    write_local: bool,
    push: bool,
    on_progress_sync: Option<ProgressSyncListener>,
}

/// A progress value this device has written (or pushed) — see `ProgressWriter`.
#[derive(Clone)]
struct WrittenProgress {
    item_id: String,
    position: f64,
    is_finished: bool,
}

impl WrittenProgress {
    fn matches(&self, item_id: &str, position: f64, is_finished: bool) -> bool {
        self.item_id == item_id && self.is_finished == is_finished && (self.position - position).abs() < 0.5
    }
}

/// Runs progress writes on two queues, each one write at a time in the order they were asked
/// for: local rows (fast, and the source of truth Home reads) and pushes to the server (slow, and
/// able to hang for the HTTP timeout). A single queue had a hanging push delay every later local
/// write — and the final one at shutdown — behind it. A push always starts after its own local
/// write has landed, so the row it marks as pushed is there. Each write used to be its own
/// spawned future, so a slow periodic push could reach the server after the pause push that
/// followed it and leave the server behind; a queued write that hasn't started yet is replaced
/// by a newer one for the same item.
///
/// Also remembers the last value written locally and the last one the server confirmed, so a
/// write of the same value again (a second pause, a new book started over a paused one) is
/// skipped. Re-pushing an unchanged, long-paused position would drag the server back over
/// progress made since on another device.
#[derive(Clone, Default)]
struct ProgressWriter {
    local_queue: Rc<RefCell<std::collections::VecDeque<ProgressWrite>>>,
    local_running: Rc<std::cell::Cell<bool>>,
    push_queue: Rc<RefCell<std::collections::VecDeque<ProgressWrite>>>,
    push_running: Rc<std::cell::Cell<bool>>,
    last_local: Rc<RefCell<Option<WrittenProgress>>>,
    last_pushed: Rc<RefCell<Option<WrittenProgress>>>,
    /// The push currently running, if any — a push of the same value asked for meanwhile is a
    /// duplicate too.
    in_flight_push: Rc<RefCell<Option<WrittenProgress>>>,
}

/// Puts `write` last in `queue`, replacing a queued write for the same item and keeping what
/// that one was going to do.
fn coalesce_into(queue: &RefCell<std::collections::VecDeque<ProgressWrite>>, write: ProgressWrite) {
    let mut queue = queue.borrow_mut();
    let mut write = write;
    queue.retain(|queued| {
        if queued.item_id != write.item_id {
            return true;
        }
        write.write_local |= queued.write_local;
        write.push |= queued.push;
        false
    });
    queue.push_back(write);
}

impl ProgressWriter {
    fn is_written_locally(&self, item_id: &str, position: f64, is_finished: bool) -> bool {
        self.last_local.borrow().as_ref().is_some_and(|w| w.matches(item_id, position, is_finished))
    }

    /// Whether the server has (or is about to have, from a push already queued or running) this
    /// value.
    fn is_pushed(&self, item_id: &str, position: f64, is_finished: bool) -> bool {
        let queued = |queue: &RefCell<std::collections::VecDeque<ProgressWrite>>| {
            queue.borrow().iter().any(|w| w.push && w.item_id == item_id && w.is_finished == is_finished && (w.position - position).abs() < 0.5)
        };
        self.last_pushed.borrow().as_ref().is_some_and(|w| w.matches(item_id, position, is_finished))
            || self.in_flight_push.borrow().as_ref().is_some_and(|w| w.matches(item_id, position, is_finished))
            || queued(&self.local_queue)
            || queued(&self.push_queue)
    }

    /// Makes the next write push even if the server already had that value — for a deliberate
    /// "put it back" (Undo) after another device's position was adopted.
    fn forget_pushed(&self) {
        self.last_pushed.borrow_mut().take();
        self.in_flight_push.borrow_mut().take();
    }

    fn enqueue(&self, write: ProgressWrite) {
        if !write.write_local {
            self.enqueue_push(write);
            return;
        }
        coalesce_into(&self.local_queue, write);
        if self.local_running.get() {
            return;
        }
        self.local_running.set(true);
        let writer = self.clone();
        glib::spawn_future_local(async move {
            loop {
                let next = writer.local_queue.borrow_mut().pop_front();
                let Some(next) = next else { break };
                writer.run_local(next).await;
            }
            writer.local_running.set(false);
        });
    }

    fn enqueue_push(&self, write: ProgressWrite) {
        if !write.push {
            return;
        }
        coalesce_into(&self.push_queue, write);
        if self.push_running.get() {
            return;
        }
        self.push_running.set(true);
        let writer = self.clone();
        glib::spawn_future_local(async move {
            loop {
                let next = writer.push_queue.borrow_mut().pop_front();
                let Some(next) = next else { break };
                writer.run_push(next).await;
            }
            writer.push_running.set(false);
        });
    }

    /// Runs whatever is queued to completion, blocking, for at most `timeout` — for app
    /// shutdown, where a spawned future would never get to run. The local writes go first and
    /// in full; only a slow push is cut short (its row stays marked for the next launch's sync).
    fn drain_blocking(&self, timeout: Duration) {
        let writer = self.clone();
        let drained = glib::MainContext::default().block_on(glib::future_with_timeout(timeout, async move {
            // While a queue's own runner (spawned by `enqueue`) is active, let it finish the
            // queue — `block_on` keeps iterating the main context — rather than taking writes
            // from under it, which could run two at once and land them out of order.
            loop {
                if writer.local_running.get() {
                    glib::timeout_future(Duration::from_millis(10)).await;
                    continue;
                }
                let next = writer.local_queue.borrow_mut().pop_front();
                let Some(next) = next else { break };
                writer.run_local(next).await;
            }
            loop {
                if writer.push_running.get() {
                    glib::timeout_future(Duration::from_millis(10)).await;
                    continue;
                }
                let next = writer.push_queue.borrow_mut().pop_front();
                let Some(next) = next else { break };
                writer.run_push(next).await;
            }
        }));
        if drained.is_err() {
            tracing::warn!("gave up waiting for the last progress push at shutdown; it will be pushed on the next launch");
        }
    }

    /// The local half of a write, then hands the push half (if any) to the push queue.
    async fn run_local(&self, write: ProgressWrite) {
        if write.write_local {
            match abs_storage::repo::progress::set(&write.pool, &write.account_id, &write.server_id, &write.item_id, write.position, write.is_finished).await {
                Ok(()) => {
                    *self.last_local.borrow_mut() =
                        Some(WrittenProgress { item_id: write.item_id.clone(), position: write.position, is_finished: write.is_finished })
                }
                Err(err) => tracing::warn!(%err, "couldn't persist playback progress"),
            }
        }
        self.enqueue_push(write);
    }

    async fn run_push(&self, write: ProgressWrite) {
        let ProgressWrite { pool, session, account_id, server_id, item_id, position, is_finished, duration_seconds, on_progress_sync, .. } = write;
        *self.in_flight_push.borrow_mut() = Some(WrittenProgress { item_id: item_id.clone(), position, is_finished });
        self.push(pool, session, account_id, server_id, item_id, position, is_finished, duration_seconds, on_progress_sync).await;
        self.in_flight_push.borrow_mut().take();
    }

    #[allow(clippy::too_many_arguments)]
    async fn push(
        &self,
        pool: SqlitePool,
        session: abs_core::auth::Session,
        account_id: String,
        server_id: String,
        item_id: String,
        position: f64,
        is_finished: bool,
        duration_seconds: f64,
        on_progress_sync: Option<ProgressSyncListener>,
    ) {
        // Best-effort: the local write above is this client's own source of truth (Home's
        // "Continue Listening" reads it), so a network hiccup syncing it up to the server
        // must not be treated as a playback error. `Session::api_client` asks for a fresh
        // token/connection on every call (so a settings change or a token refresh is always
        // honored) but reuses the already-minted `abs_api::Client` — and the TLS connection it
        // holds open — as long as neither has actually changed, which for this call site (the
        // most frequent server-facing one in the app, while playing) is what keeps a periodic
        // background sync from costing a fresh TLS handshake every time.
        let report = |outcome| {
            if let Some(on_progress_sync) = &on_progress_sync {
                on_progress_sync(outcome);
            }
        };
        let outcome_of = |err: &abs_core::CoreError| match err {
            abs_core::CoreError::Auth => ProgressSyncOutcome::SessionExpired,
            _ => ProgressSyncOutcome::Failed,
        };
        let api = match session.api_client().await {
            Ok(api) => api,
            // Kept locally (the row stays marked for pushing) until offline mode is switched off,
            // which pushes it — not a sync failure to tell anyone about.
            Err(abs_core::CoreError::Offline) => return,
            Err(err) => {
                tracing::warn!(%err, "couldn't build an API client for this server; progress stays local");
                report(outcome_of(&err));
                return;
            }
        };
        match abs_core::streaming::sync_progress_to_server_with_client(&api, &item_id, position, duration_seconds, is_finished).await {
            Ok(()) => {
                if let Err(err) = abs_storage::repo::progress::mark_pushed(&pool, &account_id, &server_id, &item_id, position, is_finished).await {
                    tracing::warn!(%err, "couldn't record that progress reached the server; it will be pushed again");
                }
                *self.last_pushed.borrow_mut() = Some(WrittenProgress { item_id, position, is_finished });
                report(ProgressSyncOutcome::Synced);
            }
            Err(err) => {
                tracing::warn!(%err, "couldn't sync playback progress to the server");
                report(outcome_of(&err));
            }
        }
    }
}


struct Inner {
    backend: Box<dyn abs_player::AudioBackend>,
    pool: SqlitePool,
    paths: AppPaths,
    now_playing: Option<NowPlaying>,
    /// See [`PendingStart`]. `Some` exactly while a `start()` is resolving; `now_playing` is
    /// `None` meanwhile.
    pending_start: Option<PendingStart>,
    /// Bumped by every `start()`. Async work that belongs to one start (the start itself, a
    /// resume's reconcile, the cover fetch) checks it after every await and gives up once a
    /// newer start has replaced it — comparing item ids isn't enough, since the same book can be
    /// started again.
    session_generation: u64,
    /// Bumped by every load and every backend reset (`begin_load`, `reset_backend`). Async work
    /// that is about to act on the backend (a track load, a deferred seek, a pause confirmation,
    /// an error recovery) checks it first: if anything has loaded or released the pipeline since,
    /// its action would land on the wrong file.
    load_generation: u64,
    /// Bumped by every seek — a resume's reconcile doesn't override a position the listener
    /// chose while it was running.
    seek_count: u64,
    /// Permanent listeners, notified on every published snapshot for the app's whole lifetime —
    /// the mini-player bar's closure is pushed here at construction, and MPRIS (once wired) is
    /// pushed here too via `PlayerController::add_listener`. Distinct from `full_update`, the one
    /// optional slot toggled as the full player screen opens/closes.
    listeners: RefCell<Vec<SnapshotListener>>,
    /// Listeners registered from inside a publish (a listener building a screen that listens
    /// too); added once that publish is over.
    listeners_added_meanwhile: RefCell<Vec<SnapshotListener>>,
    full_update: Option<FullUpdateListener>,
    /// When the local progress row was last refreshed — the periodic tick's own throttle (see
    /// `LOCAL_PROGRESS_WRITE_INTERVAL`).
    last_progress_write: Instant,
    /// When progress was last actually pushed to the server — the periodic tick's separate,
    /// coarser throttle (see `SERVER_PROGRESS_SYNC_INTERVAL`). Every explicit `write_progress`
    /// call (pause, end-of-book, a sleep timer firing, ...) still syncs immediately and updates
    /// this too, so the tick never re-syncs something an explicit action just pushed a moment ago.
    last_server_sync: Instant,
    /// Headphone unplug behavior, from `PlaybackSettings` (Settings → Playback) — see
    /// `handle_route_event`.
    pause_on_unplug: bool,
    resume_on_replug: bool,
    /// Live playback config, also from `PlaybackSettings` (Settings → Playback): the speed every
    /// `start()` begins a session at, and the skip intervals the transport buttons, keyboard
    /// actions and MPRIS next/previous use. Held here rather than captured by the various
    /// closures at build time, so a Settings edit takes effect immediately (see
    /// `set_playback_config`).
    default_speed: f64,
    skip_back_seconds: f64,
    skip_forward_seconds: f64,
    /// Whether the current pause was caused by `handle_route_event`'s unplug handling (as opposed
    /// to a manual pause, a phone call, a sleep timer or end-of-book). Only a pause this specific
    /// may ever be lifted by a replug. Cleared by every other pause path.
    paused_by_unplug: bool,
    /// When `handle_route_event` last paused for an unplug — what
    /// `PlayerController::external_play_pause` measures [`SPURIOUS_UNPLUG_TOGGLE_WINDOW`] from.
    /// Only meaningful while `paused_by_unplug` is still set.
    unplug_paused_at: Option<Instant>,
    /// Bumped by every headphone route event: a replug resumes only if no other event came in
    /// during `REPLUG_SETTLE`.
    route_event_seq: u64,
    /// Whether the current pause was caused by `handle_call_event` — an incoming call ringing.
    /// Only a pause this specific may be lifted when that call ends unanswered. Cleared by every
    /// other pause path, and by the call being picked up.
    paused_by_call: bool,
    /// Mirrors whatever was last forwarded to `backend.set_burst_buffering` — the backend itself
    /// has no getter (it's a `Box<dyn AudioBackend>`), so this is what `PlayerController`'s own
    /// `#[cfg(test)]` getter reads to confirm Settings' switch actually reached the controller,
    /// same shape as `pause_on_unplug`/`resume_on_replug` above.
    burst_buffering: bool,
    /// Told how every server progress push ended. Without it a sync failure only reached the
    /// log, and a user could listen for hours with nothing synced and no idea.
    on_progress_sync: Option<ProgressSyncListener>,
    progress_writer: ProgressWriter,
    /// When the tick last saw something playing — how long a pause has lasted when `play()`
    /// resumes it.
    last_played_at: Instant,
    /// A resume after a pause at least this long first checks the server for newer progress
    /// (see `PlayerController::play`). A field rather than a const only so tests can shorten it.
    resume_reconcile_after: Duration,
    /// Set while that check is in flight, to the session generation it belongs to: position
    /// writes are held back, since the position may be about to move to another device's. Only
    /// the check that set it clears it.
    holding_writes_for_reconcile: Option<u64>,
    /// Told `(from, to)` when a resume adopted newer progress from the server — see
    /// `PlayerController::set_on_position_adopted`.
    on_position_adopted: Option<Rc<dyn Fn(f64, f64)>>,
    /// Bumped by every seek while paused; the delayed write only runs for the latest one.
    paused_seek_generation: u64,
    /// When a stream error last reloaded the file by itself — see `AUTO_RECOVER_WINDOW`.
    last_auto_recover_at: Option<Instant>,
    /// `STALE_CONNECTION_AFTER`, shortened by tests.
    stale_connection_after: Duration,
    /// An end-of-stream found on the bus by `play()` (which drains what arrived while paused),
    /// handed to the next tick, which is where end-of-stream is handled.
    deferred_event: Option<abs_player::PlayerEvent>,
}

impl Inner {
    fn snapshot(&self) -> Option<PlayerSnapshot> {
        if let Some(pending) = &self.pending_start {
            return Some(PlayerSnapshot {
                title: pending.title.clone(),
                author: pending.author.clone(),
                position_seconds: 0.0,
                duration_seconds: 0.0,
                is_playing: false,
                speed: self.default_speed,
                sleep_timer_active: false,
                cover_path: pending.cover_path.clone(),
                last_error: None,
                is_loading: true,
                will_play_when_loaded: pending.wants_play,
                has_chapters: false,
                chapter: None,
                source_is_local: false,
            });
        }
        let now_playing = self.now_playing.as_ref()?;
        let position = self.book_position();
        let chapter = chapter_index_at(&now_playing.chapters, position).map(|index| CurrentChapter {
            title: now_playing.chapters[index].title.clone(),
            number: index + 1,
            count: now_playing.chapters.len(),
            start_seconds: now_playing.chapters[index].start_seconds,
            end_seconds: now_playing.chapters[index].end_seconds,
        });
        Some(PlayerSnapshot {
            title: now_playing.title.clone(),
            author: now_playing.author.clone(),
            position_seconds: position,
            duration_seconds: now_playing.duration_seconds,
            is_playing: now_playing.is_playing,
            speed: now_playing.speed,
            sleep_timer_active: now_playing.sleep_timer != SleepTimerState::Off,
            cover_path: now_playing.cover_path.clone(),
            last_error: now_playing.last_error.clone(),
            is_loading: false,
            will_play_when_loaded: false,
            has_chapters: !now_playing.chapters.is_empty(),
            chapter,
            source_is_local: now_playing.current_source_is_local,
        })
    }

    /// Whether playback is on (or, while a start is resolving, will be once loaded). `None`
    /// when nothing is loaded or starting.
    fn playing_intent(&self) -> Option<bool> {
        if let Some(pending) = &self.pending_start {
            return Some(pending.wants_play);
        }
        self.now_playing.as_ref().map(|np| np.is_playing)
    }

    /// Whether `generation`'s start is still the one resolving.
    fn pending_is(&self, generation: u64) -> bool {
        self.pending_start.as_ref().is_some_and(|p| p.session_generation == generation)
    }

    /// Releases the backend (see `AudioBackend::reset`) and invalidates whatever async work was
    /// about to act on it. Every reset goes through here.
    fn reset_backend(&mut self) {
        self.backend.reset();
        self.load_generation += 1;
        if let Some(now_playing) = &mut self.now_playing {
            now_playing.loading_track = None;
        }
    }

    /// Starts a new load generation, for the load about to happen.
    fn begin_load(&mut self) -> u64 {
        self.load_generation += 1;
        self.load_generation
    }

    /// Whether the backend currently holds the current track, loaded and usable — not released
    /// after an error, and not in the middle of a track load.
    fn backend_holds_track(&self) -> bool {
        self.now_playing.as_ref().is_some_and(|np| !np.needs_reload && np.loading_track.is_none() && !np.tracks.is_empty())
    }

    /// Pauses the backend if it holds anything to pause (see `backend_holds_track`). Asking a
    /// released pipeline to pause would re-open its stale source.
    fn pause_backend_if_loaded(&mut self) {
        if self.backend_holds_track() {
            let _ = self.backend.pause();
        }
    }

    /// Switches the current track to `track_index` and starts bringing it into the backend at
    /// `within`: releases the old file right away (so it can neither keep playing nor deliver a
    /// stale end-of-stream or error against the new track), and pins the position to the target
    /// until the load lands. Returns the load generation to hand to `spawn_load_track`.
    fn prepare_track_load(&mut self, track_index: usize, within: f64) -> Option<u64> {
        self.now_playing.as_ref()?;
        self.reset_backend();
        let generation = self.begin_load();
        let now_playing = self.now_playing.as_mut()?;
        now_playing.current_track = track_index;
        now_playing.last_known_within_track = within;
        now_playing.seek_target_pending = true;
        now_playing.seek_issued_at = None;
        now_playing.seek_retries = 0;
        now_playing.loading_track = Some(generation);
        Some(generation)
    }

    /// The book-level playback position: the current track's offset within the book plus the
    /// backend's position inside that track. Progress and chapters are both defined
    /// book-level (they're shared with the server and its other clients), while the pipeline only
    /// ever knows where it is inside the single file it holds — so every read of "where are we"
    /// funnels through here rather than through `backend.position()` directly.
    ///
    /// `backend.position()` reports `None` for a real stretch of time after a `FLUSH` seek on a
    /// network-streamed source, while GStreamer waits for the new HTTP range response to arrive
    /// — on a stalled or dead connection that can be seconds or forever. Falling back to `0.0`
    /// for that window (the previous behavior) makes every consumer of this — the scrubber, the
    /// time label, the 5s local progress write, the periodic server sync, and a second skip's
    /// own `current + delta` computation — briefly (or, on a dead link, permanently) believe
    /// playback jumped to the start of the current track. `last_known_within_track` (kept fresh
    /// by `observe_position`, called every tick, and set proactively by every seek to its own
    /// target) is what a stalled query reports instead.
    fn book_position(&self) -> f64 {
        let Some(now_playing) = &self.now_playing else { return 0.0 };
        // While a seek is pending (see `seek_target_pending`'s doc comment), `backend.position()`
        // is trusted for nothing — it can be `Some` and simply stale (the pre-seek position, not
        // yet asked to move) just as easily as `None` (a network seek whose response hasn't
        // arrived yet). `last_known_within_track` is the only value known to be correct in
        // either case.
        let within_track = if now_playing.seek_target_pending {
            now_playing.last_known_within_track
        } else {
            self.backend.position().map(|d| d.as_secs_f64()).unwrap_or(now_playing.last_known_within_track)
        };
        // Never past the end of the file as the server's timeline has it (when it says): a file
        // that runs longer than reported stalls the reading at the boundary instead of showing
        // the next file's part of the book early.
        let track = now_playing.tracks.get(now_playing.current_track);
        let within_track = match track {
            Some(t) if t.duration_seconds > 0.0 => within_track.min(t.duration_seconds),
            _ => within_track,
        };
        track.map(|t| t.offset_seconds).unwrap_or(0.0) + within_track
    }

    /// Refreshes `last_known_within_track` from the backend's own position, when it has one and
    /// (while a seek is pending) it actually agrees with the seek's own target — never from a
    /// stale pre-seek reading that just happens to be `Some`. Called once per tick — the only
    /// place besides an explicit seek that keeps this fallback from going stale during ordinary,
    /// uninterrupted playback, and the only place that ever clears `seek_target_pending`.
    ///
    /// A seek that still hasn't landed after its re-issues is never given up on by adopting the
    /// pipeline's position — on a fresh load that is the start of the file, and the next progress
    /// write would save (and push) it over the listener's real position. The file is loaded again
    /// at the target instead, once: the returned `(item_id, load generation)` is for the caller to
    /// hand to `spawn_load_track`. If that doesn't land either, playback stops with an error and
    /// the target stays the position shown and saved.
    fn observe_position(&mut self) -> Option<(String, u64)> {
        // A generous tolerance: burst-buffering and container framing mean a landed seek rarely
        // reports the exact requested second, and the only failure mode of being too generous
        // here is trusting a fresher position slightly sooner — never trusting a stale one, since
        // a genuinely stale reading (the old, pre-seek position) is normally seconds away from a
        // skip/seek's target, not fractions.
        const SEEK_LANDED_TOLERANCE_SECONDS: f64 = 1.5;
        enum GiveUp {
            Reload { track: usize, target: f64, item_id: String },
            Stop { target: f64 },
        }
        let within = self.backend.position().map(|d| d.as_secs_f64())?;
        let file_end = self.backend.duration().map(|d| d.as_secs_f64()).filter(|d| *d > 0.0);
        let now_playing = self.now_playing.as_mut()?;
        // Mid-load, the target hasn't been asked of the new pipeline yet (`spawn_load_track`
        // does that once it's ready, reading it back from `last_known_within_track`), so the
        // fresh pipeline's ~0.0 can't mean the seek landed or failed.
        if now_playing.loading_track.is_some() {
            return None;
        }
        let mut give_up = None;
        if now_playing.seek_target_pending {
            let target = now_playing.last_known_within_track;
            // A target past the file's real end (the server reported it longer than it is) lands
            // at that end: the pipeline can't go further, and that's not a failed seek.
            let clamped_at_end = file_end.is_some_and(|end| target > end && (within - end).abs() <= SEEK_LANDED_TOLERANCE_SECONDS);
            // A freshly loaded file reports exactly its start until the seek lands, which for a
            // target under the tolerance would otherwise read as landed — and pin the position
            // back to the start of the file. Accurate seeks don't land on exactly zero instead.
            let still_at_start = within == 0.0 && target > 0.25;
            if ((within - target).abs() > SEEK_LANDED_TOLERANCE_SECONDS || still_at_start) && !clamped_at_end {
                // The pipeline reports a real position, just not the requested one. Either the
                // seek is still on its way, or it was asked for before the pipeline could take
                // it and silently did nothing. Give an issued seek a moment, then ask again.
                let issued_at = now_playing.seek_issued_at?;
                if issued_at.elapsed() < SEEK_REISSUE_AFTER {
                    return None;
                }
                if now_playing.seek_retries < MAX_SEEK_REISSUES {
                    now_playing.seek_retries += 1;
                    now_playing.seek_issued_at = Some(Instant::now());
                    tracing::info!(target, within, "a seek didn't land; asking again");
                    self.drop_stale_end_of_stream();
                    let _ = self.backend.seek(Duration::from_secs_f64(target));
                    return None;
                }
                give_up = Some(if now_playing.seek_reload_used {
                    GiveUp::Stop { target }
                } else {
                    GiveUp::Reload { track: now_playing.current_track, target, item_id: now_playing.item_id.clone() }
                });
            } else {
                now_playing.seek_target_pending = false;
                now_playing.seek_issued_at = None;
                now_playing.seek_retries = 0;
                now_playing.seek_reload_used = false;
            }
        }
        match give_up {
            None => {
                now_playing.last_known_within_track = within;
                None
            }
            Some(GiveUp::Reload { track, target, item_id }) => {
                tracing::warn!(target, within, "a seek never landed; reloading the file at the target");
                let generation = self.prepare_track_load(track, target)?;
                if let Some(now_playing) = &mut self.now_playing {
                    now_playing.seek_reload_used = true;
                }
                Some((item_id, generation))
            }
            Some(GiveUp::Stop { target }) => {
                tracing::warn!(target, within, "a seek never landed, even after reloading the file; stopping");
                self.reset_backend();
                if let Some(now_playing) = &mut self.now_playing {
                    // Still pending, at the target: that's what is shown, saved, and reloaded at.
                    now_playing.is_playing = false;
                    now_playing.needs_reload = true;
                    now_playing.seek_issued_at = None;
                    now_playing.last_error = Some(abs_player::PlaybackError {
                        kind: abs_player::PlaybackErrorKind::Seek,
                        message: format!("the audio never got to {target:.1}s into the file"),
                        debug: None,
                    });
                }
                None
            }
        }
    }

    fn publish(&self) {
        // Home's and Library's progress sync leave the book the player holds to the player.
        let held = self.now_playing.as_ref().map(|np| (np.server_id.as_str(), np.account_id.as_str(), np.item_id.as_str())).or_else(|| {
            self.pending_start.as_ref().map(|p| (p.server_id.as_str(), p.account_id.as_str(), p.item_id.as_str()))
        });
        crate::sync_coordinator::set_loaded_item(held);
        let Some(snapshot) = self.snapshot() else { return };
        // A listener whose screen is gone says so and is dropped here — Item Detail builds a
        // mini bar per visit, and each used to stay registered (and keep its widgets alive) for
        // good.
        self.listeners.borrow_mut().retain(|listener| listener(&snapshot));
        let mut added = self.listeners_added_meanwhile.borrow_mut();
        self.listeners.borrow_mut().append(&mut added);
        drop(added);
        if let Some(full_update) = &self.full_update {
            full_update(&snapshot);
        }
    }

    /// Queues a progress write at the backend's current position, always pushing to the server
    /// (unless it already has this value) — for pause, a sleep timer, end-of-book and the like.
    /// For a write at an explicit position (marking finished, resetting), see
    /// `write_progress_at`. Held back while a resume's reconcile is in flight (see
    /// `PlayerController::play`): the position may be about to move to another device's.
    ///
    /// A finished book (`NowPlaying::ended`) is recorded finished, at its full duration — every
    /// one of these writes used to record it unfinished at wherever the pipeline stopped, so a
    /// pause, a quit, a book switch or a reconnect after the end un-finished it again (here and
    /// on the server).
    fn write_progress(&mut self) {
        if self.holding_writes_for_reconcile.is_some() {
            return;
        }
        self.write_progress_now();
    }

    /// `write_progress` for moments that must not be lost to a resume check still in flight: the
    /// outgoing book's flush on a book switch, quitting, the end of the book, a sleep timer. The
    /// hold exists because the position may be about to move to another device's, which only
    /// matters while the player is still going to keep playing from here.
    fn write_progress_now(&mut self) {
        let Some(now_playing) = &self.now_playing else { return };
        let (position, is_finished) = if now_playing.ended {
            (now_playing.duration_seconds, true)
        } else {
            (self.book_position(), false)
        };
        self.write_progress_at(position, is_finished, true);
    }

    /// The shared implementation behind `write_progress` (backend's current position, always
    /// forcing a server sync), `PlayerController::mark_as_finished` (`duration_seconds`),
    /// `PlayerController::reset_progress` (`0.0`), and the periodic tick's own local-only refresh
    /// (`force_server_sync: false`, throttled separately by `SERVER_PROGRESS_SYNC_INTERVAL`) —
    /// same local-write-then-maybe-sync shape in every case, differing in which position gets
    /// written and whether the server push happens unconditionally. The write itself runs on
    /// `ProgressWriter`'s queue, since every call site is a synchronous GTK signal handler or the
    /// tick timer, neither of which can await.
    ///
    /// `force_server_sync: false` does not mean "never sync" — it still syncs once
    /// `last_server_sync` is stale enough, so a long stretch of uninterrupted playback keeps
    /// drifting its server-side progress forward at that coarser cadence, even though every
    /// explicit action (pause, a sleep timer firing, ...) already resets that clock by calling
    /// `write_progress` with `true`.
    fn write_progress_at(&mut self, position: f64, is_finished: bool, force_server_sync: bool) {
        let Some(now_playing) = &self.now_playing else { return };
        // A start that failed before any track resolved never played anything of this item, so
        // it has no position of its own to record — writing one would overwrite the real one.
        if now_playing.tracks.is_empty() {
            return;
        }
        self.last_progress_write = Instant::now();
        let sync_due = force_server_sync || self.last_server_sync.elapsed() >= SERVER_PROGRESS_SYNC_INTERVAL;
        let write_local = !self.progress_writer.is_written_locally(&now_playing.item_id, position, is_finished);
        let push = sync_due && !self.progress_writer.is_pushed(&now_playing.item_id, position, is_finished);
        if sync_due {
            self.last_server_sync = Instant::now();
        }
        if !write_local && !push {
            return;
        }
        self.progress_writer.enqueue(ProgressWrite {
            pool: self.pool.clone(),
            session: now_playing.session.clone(),
            account_id: now_playing.account_id.clone(),
            server_id: now_playing.server_id.clone(),
            item_id: now_playing.item_id.clone(),
            position,
            is_finished,
            duration_seconds: now_playing.duration_seconds,
            write_local,
            push,
            on_progress_sync: self.on_progress_sync.clone(),
        });
    }

    /// Called right before a seek within the loaded file: an end-of-stream already waiting on the
    /// bus was posted before the seek and is about the position being left, not the new one (a
    /// rewind just before the end of a file used to be swallowed by it, moving on to the next
    /// file or finishing the book). An error waiting there is kept for the tick.
    fn drop_stale_end_of_stream(&mut self) {
        if self.deferred_event.is_some() {
            return;
        }
        while let Some(event) = self.backend.poll_event() {
            match event {
                abs_player::PlayerEvent::EndOfStream => tracing::info!("dropped an end-of-stream from before the seek"),
                error @ abs_player::PlayerEvent::Error(_) => {
                    self.deferred_event = Some(error);
                    return;
                }
            }
        }
    }

    /// What an end-of-stream means, given how far before the end of the track it came. A stream
    /// cut off mid-file can end like a finished one, and treating it as finished would advance to
    /// the next file (or mark the whole book finished) from the middle of this one; but durations
    /// of some files are estimates, so a small gap is the real end.
    ///
    /// - Within `PREMATURE_EOS_MIN_GAP_SECONDS` of the end: the real end.
    /// - Further, but within 10% of the track: on any file but the last, the real end (moving on
    ///   a little early beats stopping); on the last one, `Unclear` — stopping is better than
    ///   finishing the book an hour early (10% of a 10-hour single file).
    /// - Further still: `Premature`, handled as a lost connection.
    ///
    /// A second unclear or premature end at the same spot is accepted as the real end, so a
    /// wrong estimate can't trap playback in a loop.
    fn judge_end_of_stream(&mut self) -> EndOfStreamVerdict {
        let within = self.backend.position().map(|d| d.as_secs_f64());
        let backend_duration = self.backend.duration().map(|d| d.as_secs_f64()).filter(|d| *d > 0.0);
        let Some(now_playing) = &mut self.now_playing else { return EndOfStreamVerdict::RealEnd };
        let within = within.unwrap_or(now_playing.last_known_within_track);
        let server_duration = now_playing.tracks.get(now_playing.current_track).map(|t| t.duration_seconds).filter(|d| *d > 0.0);
        let Some(duration) = server_duration.or(backend_duration) else { return EndOfStreamVerdict::RealEnd };
        let is_last_track = now_playing.current_track + 1 >= now_playing.tracks.len();
        let short_by = duration - within;
        let verdict = if short_by < PREMATURE_EOS_MIN_GAP_SECONDS {
            EndOfStreamVerdict::RealEnd
        } else if short_by < duration * 0.1 {
            if is_last_track { EndOfStreamVerdict::Unclear { within, short_by } } else { EndOfStreamVerdict::RealEnd }
        } else {
            EndOfStreamVerdict::Premature
        };
        if verdict == EndOfStreamVerdict::RealEnd {
            return verdict;
        }
        let here = (now_playing.current_track, within);
        if now_playing.premature_eos_at.is_some_and(|(track, at)| track == here.0 && (at - within).abs() < 5.0) {
            tracing::warn!(within, duration, "the stream ended early at the same spot again; taking it as the real end");
            now_playing.premature_eos_at = None;
            return EndOfStreamVerdict::RealEnd;
        }
        now_playing.premature_eos_at = Some(here);
        verdict
    }

    /// At end-of-stream: if another track follows the current one, returns `(item_id, next_index)`
    /// for `spawn_load_track` — leaving `is_playing` set, since from the state machine's point of
    /// view playback continues. `None` means the item really is over and the caller should run
    /// its existing pause-and-mark-finished path.
    fn next_track_after_end_of_stream(&mut self) -> Option<(String, usize)> {
        let now_playing = self.now_playing.as_mut()?;
        let next = now_playing.current_track + 1;
        if next >= now_playing.tracks.len() {
            return None;
        }

        // The server's timeline is canonical: a file whose real length differs from the reported
        // one doesn't move the later offsets. Progress is saved against that timeline and read
        // back by every other client, so shifting it here (in memory only) made saved positions
        // map to the wrong place on the next start. `book_position` keeps the reading from
        // overshooting into the next file meanwhile.
        now_playing.current_track = next;
        // Until `spawn_load_track` swaps the file, the backend still reports the finished one's
        // position; read against the next track's offset that would overshoot. Pin it to the
        // next track's start instead.
        now_playing.last_known_within_track = 0.0;
        now_playing.seek_target_pending = true;
        now_playing.seek_issued_at = None;
        Some((now_playing.item_id.clone(), next))
    }

    /// Brings the track `prepare_track_load` just switched to (load generation `generation`) into
    /// the backend, on the GLib main loop rather than synchronously — the callers run from the
    /// tick handler or a GTK signal, which must not block on a network-streamed pipeline's
    /// preroll. Gives up silently the moment anything newer has loaded or released the backend,
    /// or another book was started: checked after every await, before touching the backend.
    ///
    /// Seeking — including a speed other than 1×, which is itself a seek-with-rate — needs the
    /// pipeline to have actually *reached* `PAUSED`, so anything needing a seek waits for
    /// `position()` to become available first. The target position and speed are read from
    /// `now_playing` only once that wait is over, so a seek or speed change asked for during the
    /// load is the one applied. Resumes playing only if the item is still flagged as playing at
    /// that point, so a pause mid-load is respected rather than overridden.
    fn spawn_load_track(inner_rc: Rc<RefCell<Inner>>, item_id: String, generation: u64) {
        // Still the load this was spawned for, on the same book.
        let current = move |inner: &Inner| {
            inner.load_generation == generation
                && inner.now_playing.as_ref().is_some_and(|np| np.item_id == item_id && np.loading_track == Some(generation))
        };
        glib::spawn_future_local(async move {
            let (pool, server_id, item_id, session, ino, track_index) = {
                let inner = inner_rc.borrow();
                if !current(&inner) {
                    return;
                }
                let Some(now_playing) = &inner.now_playing else { return };
                let Some(track) = now_playing.tracks.get(now_playing.current_track) else { return };
                (
                    inner.pool.clone(),
                    now_playing.server_id.clone(),
                    now_playing.item_id.clone(),
                    now_playing.session.clone(),
                    track.ino.clone(),
                    now_playing.current_track,
                )
            };
            // Marks the load failed: nothing usable is loaded, and the next play reloads.
            let fail = |inner: &mut Inner, error: abs_player::PlaybackError| {
                inner.reset_backend();
                if let Some(now_playing) = &mut inner.now_playing {
                    now_playing.is_playing = false;
                    now_playing.needs_reload = true;
                    now_playing.last_error = Some(error);
                }
                inner.publish();
            };
            // The connection is asked at load time — a mid-book settings change (local address,
            // headers, TLS) is honored by the next track. Failure means the server row is gone
            // (session removed underneath us); stop cleanly like a load failure.
            let offline_mode = session.is_offline();
            let connection = if offline_mode { session.local_connection_target().await } else { session.connection_target().await };
            let connection = match connection {
                Ok(connection) => connection,
                Err(err) => {
                    let mut inner = inner_rc.borrow_mut();
                    if current(&inner) {
                        tracing::warn!(%err, %item_id, track = track_index, "couldn't load the server's connection settings");
                        fail(&mut inner, abs_player::PlaybackError { kind: abs_player::PlaybackErrorKind::Network, message: err.to_string(), debug: None });
                    }
                    return;
                }
            };
            // Prefers a verifiably-downloaded local file over streaming; the streaming URL, when
            // used, is rebuilt with a current token rather than reusing whatever `resolve_stream_target`
            // baked in at resolve time — by the time a multi-file book advances (possibly hours
            // later) that one can be expired.
            let (url, is_local) = resolve_playable_url(&pool, &connection, &server_id, &item_id, &ino, &session).await;
            // Offline mode: a part of the book that isn't on the device can't be played; stop
            // there, paused, so Play once offline mode is off streams it.
            if offline_mode && !is_local {
                let mut inner = inner_rc.borrow_mut();
                if current(&inner) {
                    tracing::info!(%item_id, track = track_index, "offline mode is on and this file isn't downloaded; stopping here");
                    fail(
                        &mut inner,
                        abs_player::PlaybackError {
                            kind: abs_player::PlaybackErrorKind::Offline,
                            message: "this part of the book isn't downloaded".to_string(),
                            debug: None,
                        },
                    );
                }
                return;
            }

            let needs_seek_readiness = {
                let mut inner = inner_rc.borrow_mut();
                if !current(&inner) {
                    tracing::info!(%item_id, track = track_index, "discarded a superseded track load");
                    return;
                }
                // Transport properties must be applied before the load: GStreamer creates the
                // HTTP source during it, and source-setup reads what was last applied here.
                inner.backend.apply_connection(&playback_properties(&connection));
                if let Err(err) = inner.backend.load(&url) {
                    tracing::warn!(%err, %item_id, track = track_index, "couldn't load the track");
                    fail(&mut inner, (&err).into());
                    return;
                }
                let Some(now_playing) = &inner.now_playing else { return };
                let needs = now_playing.last_known_within_track > 0.0 || (now_playing.speed - 1.0).abs() > f64::EPSILON;
                if needs {
                    let _ = inner.backend.pause();
                }
                needs
            };

            if needs_seek_readiness {
                for _ in 0..PREROLL_WAIT_ATTEMPTS {
                    {
                        let inner = inner_rc.borrow();
                        if !current(&inner) {
                            tracing::info!(%item_id, track = track_index, "discarded a superseded track load");
                            return;
                        }
                        if inner.backend.position().is_some() {
                            break;
                        }
                    }
                    glib::timeout_future(Duration::from_millis(100)).await;
                }
            }

            let mut inner = inner_rc.borrow_mut();
            if !current(&inner) {
                tracing::info!(%item_id, track = track_index, "discarded a superseded track load");
                return;
            }
            let Some((within, speed)) = inner.now_playing.as_ref().map(|np| (np.last_known_within_track, np.speed)) else { return };
            if (speed - 1.0).abs() > f64::EPSILON {
                let _ = inner.backend.set_speed(speed, Duration::from_secs_f64(within));
            } else if within > 0.0 {
                let _ = inner.backend.seek(Duration::from_secs_f64(within));
            }
            let Some(now_playing) = &mut inner.now_playing else { return };
            now_playing.last_error = None;
            // The load above fully replaces the pipeline, so every bit of load-scoped state gets
            // set fresh here — this is the one place a reload (self-heal, Retry), a track advance
            // and a cross-track seek converge.
            now_playing.needs_reload = false;
            now_playing.loading_track = None;
            now_playing.speed_unapplied = None;
            now_playing.current_source_is_local = is_local;
            // The readiness wait above gives up after 15 s, and a seek asked for before the
            // pipeline is ready silently does nothing — so a seek is only trusted once
            // `observe_position` sees it land (and re-issued if it doesn't).
            now_playing.seek_target_pending = within > 0.0;
            now_playing.seek_issued_at = (within > 0.0).then(Instant::now);
            now_playing.seek_retries = 0;
            let is_playing = now_playing.is_playing;
            tracing::info!(%item_id, track = track_index, within, speed, local = is_local, playing = is_playing, "track loaded");
            if is_playing {
                let _ = inner.backend.play();
            }
            inner.publish();
        });
    }

    /// Hands the backend a speed picked while paused on a stream (`NowPlaying::speed_unapplied`),
    /// right before it plays. A refusal puts the speed it still runs at back on display.
    fn apply_deferred_speed(&mut self) {
        let Some(now_playing) = &mut self.now_playing else { return };
        let Some(applied) = now_playing.speed_unapplied.take() else { return };
        let speed = now_playing.speed;
        let within = now_playing.last_known_within_track;
        let within = if now_playing.seek_target_pending { within } else { self.backend.position().map(|d| d.as_secs_f64()).unwrap_or(within) };
        self.seek_count += 1;
        self.drop_stale_end_of_stream();
        if let Err(err) = self.backend.set_speed(speed, Duration::from_secs_f64(within)) {
            tracing::warn!(%err, speed, "couldn't apply the speed picked while paused; staying at the previous one");
            if let Some(now_playing) = &mut self.now_playing {
                now_playing.speed = applied;
            }
        }
    }

    /// Before a paused pipeline is asked to play again: the tick doesn't run while paused, so
    /// whatever the pipeline reported meanwhile is still on its bus. A stream error (burst
    /// buffering keeps downloading while paused) means it can't resume — it's released and
    /// marked for the reload `play()` then does, instead of erroring right after Play. An
    /// end-of-stream is handed to the next tick, which handles it as usual. And a stream paused
    /// for `stale_connection_after` is reloaded too, with a fresh connection (see
    /// `STALE_CONNECTION_AFTER`).
    fn prepare_to_resume(&mut self) {
        // While playing, the tick is watching the bus itself.
        if !self.backend_holds_track() || self.now_playing.as_ref().is_some_and(|np| np.is_playing) {
            return;
        }
        // `poll_event` skips everything but an end-of-stream or an error, so one call finds either.
        let failed = match self.backend.poll_event() {
            Some(abs_player::PlayerEvent::Error(err)) => Some(err),
            Some(abs_player::PlayerEvent::EndOfStream) => {
                self.deferred_event = Some(abs_player::PlayerEvent::EndOfStream);
                return;
            }
            None => None,
        };
        let Some(now_playing) = &self.now_playing else { return };
        let stale = !now_playing.current_source_is_local && self.last_played_at.elapsed() >= self.stale_connection_after;
        if failed.is_none() && !stale {
            return;
        }
        let within = if now_playing.seek_target_pending {
            now_playing.last_known_within_track
        } else {
            self.backend.position().map(|d| d.as_secs_f64()).unwrap_or(now_playing.last_known_within_track)
        };
        match &failed {
            Some(err) => tracing::info!(kind = ?err.kind, within, "the stream failed while paused; reloading it to resume"),
            None => tracing::info!(paused_for = ?self.last_played_at.elapsed(), within, "resuming with a fresh connection"),
        }
        self.reset_backend();
        if let Some(now_playing) = &mut self.now_playing {
            now_playing.needs_reload = true;
            now_playing.last_known_within_track = within;
        }
    }

    /// Logs once if the load `generation` (an automatic reload after a stream error) still has
    /// no audio after `STREAM_SLOW_TO_ANSWER_AFTER` — the stream's own timeout reports the error
    /// later, and meanwhile the player says it's playing.
    fn log_if_the_stream_is_slow_to_answer(inner_rc: Rc<RefCell<Inner>>, generation: u64) {
        let inner_rc = Rc::downgrade(&inner_rc);
        glib::timeout_add_local_once(STREAM_SLOW_TO_ANSWER_AFTER, move || {
            let Some(inner_rc) = inner_rc.upgrade() else { return };
            let inner = inner_rc.borrow();
            let waiting = inner.now_playing.as_ref().is_some_and(|np| np.loading_track == Some(generation))
                || (inner.load_generation == generation && inner.backend.position().is_none());
            if waiting {
                tracing::warn!(waited = ?STREAM_SLOW_TO_ANSWER_AFTER, "still waiting for the stream after reloading it");
            }
        });
    }

    /// Confirms a `pause()` call whose `backend.pause()` returned `Ok` actually landed — see
    /// `AudioBackend::pause`'s doc comment for why `Ok` alone doesn't mean that. Polls
    /// `backend.is_paused()` on the GTK main loop (never blocking it, unlike a real
    /// `pipeline.state(timeout)` wait would) every [`PAUSE_CONFIRM_INTERVAL`] for up to
    /// [`PAUSE_CONFIRM_ATTEMPTS`] tries.
    ///
    /// Bails out quietly the moment there is nothing left for it to confirm: nothing is loaded
    /// any more, playback resumed (a legitimate `play()` in the meantime is not a race this poll
    /// needs to win against), or the backend was already reloaded/reset by something else. Only
    /// escalates — the same recovery `backend.pause()`'s own `Err` branch already uses: mark
    /// `needs_reload`, set a friendly `last_error`, release the backend — if none of that
    /// happened and `is_paused()` never once came back true across the whole window: a pipeline
    /// stuck in GStreamer's `Async` state, which on a network-streamed source mid-buffer-read can
    /// persist far longer than this window, or effectively forever (the Librem 5 field report
    /// this fixes: `pause()` logged success and audio kept playing, indefinitely, through the
    /// speaker).
    ///
    /// `generation` is the load generation the pause was asked of: a load or reset since leaves
    /// the pipeline not paused for reasons of its own, which is not this pause failing. Likewise
    /// `seek_count`: a seek or speed change since (pause, then rewind) makes the pipeline preroll
    /// again, which on a stream takes as long as the new range request — and if that seek met a
    /// pause still in flight, the pipeline only settles once the stream answers.
    fn spawn_pause_confirmation(inner_rc: Rc<RefCell<Inner>>, generation: u64, seek_count: u64) {
        glib::spawn_future_local(async move {
            for attempt in 0..PAUSE_CONFIRM_ATTEMPTS {
                glib::timeout_future(PAUSE_CONFIRM_INTERVAL).await;

                let still_pending = {
                    let inner = inner_rc.borrow();
                    if inner.load_generation != generation {
                        return;
                    }
                    if inner.seek_count != seek_count {
                        tracing::info!("pause confirmation ended by a seek");
                        return;
                    }
                    let Some(now_playing) = &inner.now_playing else { return };
                    if now_playing.is_playing || now_playing.needs_reload {
                        return;
                    }
                    !inner.backend.is_paused()
                };
                if !still_pending {
                    return;
                }

                if attempt + 1 == PAUSE_CONFIRM_ATTEMPTS {
                    let mut inner = inner_rc.borrow_mut();
                    tracing::warn!(
                        waited = ?(PAUSE_CONFIRM_INTERVAL * PAUSE_CONFIRM_ATTEMPTS),
                        "pause() never reached Paused; resetting the pipeline and marking it for reload"
                    );
                    if let Some(now_playing) = &mut inner.now_playing {
                        now_playing.last_error = Some(abs_player::PlaybackError {
                            kind: abs_player::PlaybackErrorKind::AudioOutput,
                            message: "the audio pipeline never actually paused".to_string(),
                            debug: None,
                        });
                        now_playing.needs_reload = true;
                    }
                    inner.reset_backend();
                    inner.publish();
                }
            }
        });
    }
}

/// See `PlayerController::downgrade`.
pub struct WeakPlayerController {
    inner: std::rc::Weak<RefCell<Inner>>,
    tick_source: std::rc::Weak<RefCell<Option<glib::SourceId>>>,
}

impl WeakPlayerController {
    pub fn upgrade(&self) -> Option<PlayerController> {
        Some(PlayerController { inner: self.inner.upgrade()?, tick_source: self.tick_source.upgrade()? })
    }
}

#[derive(Clone)]
pub struct PlayerController {
    inner: Rc<RefCell<Inner>>,
    /// `Some` exactly while the tick timer is installed. Unlike most of this workspace's other
    /// permanent GLib sources, this one is **not** left running for the controller's whole
    /// lifetime: a paused (or nothing-loaded) player has nothing that changes on its own between
    /// user actions, so ticking it 4 times a second would just be a battery-draining no-op. See
    /// `ensure_ticking`/`tick` for the install/stop halves of this. Tests that build a controller
    /// and start something playing should call `stop()` once done, so its timer doesn't keep
    /// firing forever on the shared GLib main context, interleaving with whatever scenario runs
    /// next.
    tick_source: Rc<RefCell<Option<glib::SourceId>>>,
}

impl PlayerController {
    pub fn new(
        pool: SqlitePool,
        paths: AppPaths,
        backend: Box<dyn abs_player::AudioBackend>,
        mini_update: impl Fn(&PlayerSnapshot) + 'static,
    ) -> Self {
        // No tick timer installed yet — nothing is loaded, so there's nothing to tick. See
        // `ensure_ticking`, called from every path that starts (or resumes) playback.
        Self {
            inner: Rc::new(RefCell::new(Inner {
                backend,
                pool,
                paths,
                now_playing: None,
                pending_start: None,
                session_generation: 0,
                load_generation: 0,
                seek_count: 0,
                listeners: RefCell::new(vec![Box::new(move |snapshot: &PlayerSnapshot| {
                    mini_update(snapshot);
                    true
                })]),
                listeners_added_meanwhile: RefCell::new(Vec::new()),
                full_update: None,
                last_progress_write: Instant::now(),
                last_server_sync: Instant::now(),
                // Overridden right after construction via `set_headphone_behavior` and
                // `set_playback_config` (the settings aren't known to `new()`'s signature)
                // — false/false is the safe "do nothing automatically" middle, and the
                // playback defaults below are `PlaybackSettings`' own defaults.
                pause_on_unplug: false,
                resume_on_replug: false,
                default_speed: abs_core::playback::DEFAULT_SPEED,
                skip_back_seconds: 15.0,
                skip_forward_seconds: 30.0,
                paused_by_unplug: false,
                unplug_paused_at: None,
                route_event_seq: 0,
                paused_by_call: false,
                // Matches `PlaybackSettings::default().burst_buffering` — overridden right after
                // construction the same way as the fields above, via `set_burst_buffering`.
                burst_buffering: true,
                on_progress_sync: None,
                progress_writer: ProgressWriter::default(),
                last_played_at: Instant::now(),
                resume_reconcile_after: RESUME_RECONCILE_AFTER,
                holding_writes_for_reconcile: None,
                on_position_adopted: None,
                paused_seek_generation: 0,
                last_auto_recover_at: None,
                stale_connection_after: STALE_CONNECTION_AFTER,
                deferred_event: None,
            })),
            tick_source: Rc::new(RefCell::new(None)),
        }
    }

    /// Installs the tick timer if it isn't already running — idempotent, so every call site that
    /// starts or resumes playback (and `set_sleep_timer_minutes`, for a wall-clock deadline that
    /// must keep counting down even while paused) can just call this unconditionally. `tick`
    /// itself is what removes the source again once neither condition holds — see its doc
    /// comment — so this is the only place that ever re-installs it.
    fn ensure_ticking(&self) {
        if self.tick_source.borrow().is_some() {
            return;
        }
        let controller = self.clone();
        let source_id = glib::timeout_add_local(TICK_INTERVAL, move || {
            if controller.tick() {
                glib::ControlFlow::Continue
            } else {
                // The source is about to self-destroy (returning `Break` below) — clear our own
                // handle to it *before* that happens, so nothing later mistakes it for still
                // being alive and calls `.remove()` on an id GLib is about to invalidate. Same
                // footgun `Debouncer::schedule`'s doc comment explains in detail.
                controller.tick_source.borrow_mut().take();
                glib::ControlFlow::Break
            }
        });
        *self.tick_source.borrow_mut() = Some(source_id);
    }

    /// Stops the tick timer, if one is running. Production code never needs this explicitly —
    /// `tick` already stops it on its own once nothing justifies it — but tests that build a
    /// controller and start something playing should call it once done, so its timer doesn't
    /// keep running into later scenarios.
    #[cfg(test)]
    pub fn stop(&self) {
        if let Some(id) = self.tick_source.borrow_mut().take() {
            id.remove();
        }
    }

    /// Takes this controller out of service for good — the shell that owned it is being replaced
    /// (an account switch, a sign-out). Saves the book's position, stops its audio, invalidates
    /// anything still on its way back from the network, and drops every listener (which also
    /// drops the MPRIS registration the shell's listener owns), so media keys and the lock-screen
    /// card can no longer drive a player nobody sees any more.
    pub fn retire(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.now_playing.is_some() {
                inner.write_progress_now();
            }
            inner.session_generation += 1;
            inner.reset_backend();
            inner.now_playing = None;
            inner.pending_start = None;
            inner.listeners.borrow_mut().clear();
            inner.full_update = None;
        }
        if let Some(id) = self.tick_source.borrow_mut().take() {
            id.remove();
        }
        crate::sync_coordinator::set_loaded_item(None);
        tracing::info!("retired the previous player");
    }

    pub fn set_full_update(&self, update: impl Fn(&PlayerSnapshot) + 'static) {
        self.inner.borrow_mut().full_update = Some(Box::new(update));
    }

    pub fn clear_full_update(&self) {
        self.inner.borrow_mut().full_update = None;
    }

    /// Registers a permanent snapshot listener, notified alongside the mini-player bar's for the
    /// app's whole lifetime — unlike `set_full_update`, this has no corresponding "clear" (nothing
    /// needs to stop listening once registered; MPRIS is the first user of this).
    pub fn add_listener(&self, listener: impl Fn(&PlayerSnapshot) + 'static) {
        self.add_scoped_listener(move |snapshot| {
            listener(snapshot);
            true
        });
    }

    #[cfg(test)]
    pub(crate) fn listener_count(&self) -> usize {
        self.inner.borrow().listeners.borrow().len()
    }

    /// Like `add_listener`, for a listener tied to a screen that comes and goes: it returns
    /// `false` once that screen is gone and is then dropped.
    pub fn add_scoped_listener(&self, listener: impl Fn(&PlayerSnapshot) -> bool + 'static) {
        let inner = self.inner.borrow();
        let listener = Box::new(listener);
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

    pub fn snapshot(&self) -> Option<PlayerSnapshot> {
        self.inner.borrow().snapshot()
    }

    /// The item id currently loaded (playing or paused), if any — lets a caller that only knows
    /// an item id (Item Detail) decide whether acting on "the current item" (`mark_as_finished`,
    /// `reset_progress`, below) would actually affect the item it means, since neither of those
    /// checks the id itself.
    pub fn current_item_id(&self) -> Option<String> {
        let inner = self.inner.borrow();
        inner.now_playing.as_ref().map(|np| np.item_id.clone()).or_else(|| inner.pending_start.as_ref().map(|p| p.item_id.clone()))
    }

    /// The currently-playing item's chapters, if any — for the chapters sheet. Empty if nothing
    /// is playing or the item has no chapter data.
    pub fn chapters(&self) -> Vec<ChapterInfo> {
        self.inner.borrow().now_playing.as_ref().map(|np| np.chapters.clone()).unwrap_or_default()
    }

    /// Whether the loaded book has chapter data — cheaper than `chapters()` for a yes/no.
    pub fn has_chapters(&self) -> bool {
        self.inner.borrow().now_playing.as_ref().is_some_and(|np| !np.chapters.is_empty())
    }

    /// Where the app keeps its files — for screens that open a book's cached files (the cover
    /// viewer).
    pub fn paths(&self) -> AppPaths {
        self.inner.borrow().paths.clone()
    }

    /// `(session, server_id, item_id)` for whatever is currently loaded — the context a download
    /// button needs to call `DownloadManager::start_download`. `None` if nothing is playing.
    pub fn current_download_context(&self) -> Option<(abs_core::auth::Session, String, String)> {
        let inner = self.inner.borrow();
        let Some(now_playing) = inner.now_playing.as_ref() else {
            return inner.pending_start.as_ref().map(|p| (p.session.clone(), p.server_id.clone(), p.item_id.clone()));
        };
        Some((now_playing.session.clone(), now_playing.server_id.clone(), now_playing.item_id.clone()))
    }

    /// Index into `chapters()` that the current book-level position falls in — the same
    /// `start_seconds <= position && position < end_seconds` test `build_chapter_row` uses to
    /// highlight "the current chapter", pulled out here so a download button can default its
    /// scope to it too. `None` if nothing is playing or the item has no chapter data; a position
    /// past every chapter's range (a rare rounding edge) clamps to the last chapter.
    pub fn current_chapter_index(&self) -> Option<usize> {
        let inner = self.inner.borrow();
        let now_playing = inner.now_playing.as_ref()?;
        chapter_index_at(&now_playing.chapters, inner.book_position())
    }

    /// Pauses (if playing) and marks the current item finished at its full duration — the same
    /// end state reaching the actual end of the book leaves it in, triggered manually. Useful for
    /// testing (no more manually scrubbing to the end or editing the DB by hand) and a real,
    /// shippable action in its own right — Audiobookshelf's other clients let you do the same. A
    /// no-op if nothing is playing.
    pub fn mark_as_finished(&self) -> bool {
        let mut inner = self.inner.borrow_mut();
        if let Some(pending) = &mut inner.pending_start {
            tracing::info!(item_id = %pending.item_id, "mark as finished: will apply once loaded");
            pending.intent = Some(StartIntent::MarkFinished);
            return true;
        }
        let Some(duration_seconds) = inner.now_playing.as_ref().map(|np| np.duration_seconds) else { return false };
        tracing::info!("marking the current book finished");
        inner.pause_backend_if_loaded();
        if let Some(now_playing) = &mut inner.now_playing {
            now_playing.is_playing = false;
            // Every later write (a quit, a switch) keeps it finished, and Play starts it over.
            now_playing.ended = true;
        }
        inner.paused_by_unplug = false;
        inner.paused_by_call = false;
        inner.publish();
        inner.write_progress_at(duration_seconds, true, true);
        true
    }

    /// Resets the current item's progress back to the start — local and server — and seeks
    /// playback there too, so the effect is visible immediately without reopening anything.
    /// Useful for testing (repeatedly restarting a book from scratch) and, per the same reasoning
    /// as `mark_as_finished`, worth keeping as real functionality rather than a debug-only
    /// backdoor. A no-op if nothing is playing.
    pub fn reset_progress(&self) -> bool {
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(pending) = &mut inner.pending_start {
                tracing::info!(item_id = %pending.item_id, "reset progress: will apply once loaded");
                pending.intent = Some(StartIntent::Reset);
                return true;
            }
            if inner.now_playing.is_none() {
                return false;
            }
        }
        // Book position 0 is track 0's start — which is a cross-track seek whenever a later file
        // is loaded, so this goes through `seek_to_seconds`'s mapping rather than the backend
        // directly.
        self.seek_to_seconds(0.0);
        let mut inner = self.inner.borrow_mut();
        inner.publish();
        inner.write_progress_at(0.0, false, true);
        true
    }

    /// Resolves a playable URL and starts playback, resuming from any existing progress for this
    /// item/account rather than always starting over from position 0. `default_speed` is
    /// `PlaybackSettings::default_speed`, applied once at the start of every session (the user can
    /// change it afterwards via `set_speed`).
    pub fn start(&self, session: abs_core::auth::Session, item: PlayRequest, default_speed: f64) {
        self.start_with(session, item, default_speed, None);
    }

    /// `start`, beginning at chapter `start_chapter` (when it exists) instead of the saved
    /// position. The chapter is applied as part of the start, so nothing seeks a book that
    /// isn't loaded yet.
    ///
    /// The previous book is stopped and dropped right away, and the new one is pending (see
    /// [`PendingStart`]) until it has resolved and loaded. Starting the book that is already
    /// pending only updates it. Starting the book that is already *loaded* doesn't reload it: it
    /// seeks to the chapter, if one was asked for, and plays — `play()` starts a finished book
    /// over, reloads after an error, and checks for progress made elsewhere after a long pause.
    /// Only a book whose start failed before anything resolved is started again from scratch.
    pub fn start_with(&self, session: abs_core::auth::Session, item: PlayRequest, default_speed: f64, start_chapter: Option<usize>) {
        let default_speed = usable_speed(default_speed).unwrap_or(abs_core::playback::DEFAULT_SPEED);
        let same_book = |item_id: &str, server_id: &str, account_id: &str| {
            item_id == item.item_id && server_id == session.server_id() && account_id == session.account_id()
        };
        let loaded_chapter_start = {
            let inner = self.inner.borrow();
            inner.now_playing.as_ref().filter(|np| {
                same_book(&np.item_id, &np.server_id, &np.account_id) && !np.tracks.is_empty() && np.retry_request.is_none()
            }).map(|np| start_chapter.and_then(|index| np.chapters.get(index)).map(|c| c.start_seconds.max(0.0)))
        };
        if let Some(chapter_start) = loaded_chapter_start {
            tracing::info!(item_id = %item.item_id, ?start_chapter, "already loaded; playing");
            // The chapter first, so the position is pinned before the pipeline is asked to play.
            if let Some(at) = chapter_start {
                self.seek_to_seconds(at);
            }
            self.play();
            return;
        }
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(pending) = &mut inner.pending_start {
                if same_book(&pending.item_id, &pending.server_id, &pending.account_id) {
                    pending.wants_play = true;
                    if start_chapter.is_some() {
                        pending.start_chapter = start_chapter;
                    }
                    tracing::info!(item_id = %item.item_id, ?start_chapter, "this book is already starting");
                    inner.publish();
                    return;
                }
            }
        }
        let generation = {
            let mut inner = self.inner.borrow_mut();
            // Flush the outgoing book's position before it's dropped — otherwise switching while
            // it plays loses up to `LOCAL_PROGRESS_WRITE_INTERVAL` of it.
            if inner.now_playing.is_some() {
                inner.write_progress_now();
            }
            let previous = inner.now_playing.as_ref().map(|np| np.item_id.clone());
            inner.session_generation += 1;
            let generation = inner.session_generation;
            // Stops the outgoing book's audio now, and invalidates anything still on its way to
            // the backend for it (a track load, a deferred seek).
            inner.reset_backend();
            inner.now_playing = None;
            // Marks belong to the outgoing book's pause; nothing may resume this one for them.
            inner.paused_by_unplug = false;
            inner.paused_by_call = false;
            inner.holding_writes_for_reconcile = None;
            inner.pending_start = Some(PendingStart {
                session_generation: generation,
                item_id: item.item_id.clone(),
                server_id: session.server_id().to_string(),
                account_id: session.account_id().to_string(),
                title: item.title.clone(),
                author: item.author.clone(),
                cover_path: None,
                wants_play: true,
                start_chapter,
                intent: None,
                session: session.clone(),
            });
            tracing::info!(item_id = %item.item_id, ?previous, ?start_chapter, session = generation, "starting playback");
            inner.publish();
            generation
        };
        let inner_rc = self.inner.clone();
        let controller = self.clone();
        glib::spawn_future_local(async move {
            // Every await below is followed by this check: a newer start replaces this one, and
            // from then on nothing here may touch the backend or the player state.
            let superseded = |inner: &Inner| {
                let stale = !inner.pending_is(generation);
                if stale {
                    tracing::info!(item_id = %item.item_id, session = generation, "discarded a superseded start");
                }
                stale
            };
            let (pool, paths) = {
                let inner = inner_rc.borrow();
                (inner.pool.clone(), inner.paths.clone())
            };

            // The cover the player shows is, first of all, whatever is already cached locally —
            // the same `cover_cache_path` Home/Library render from — so it appears while the
            // book is still resolving. The download (if any) runs once the book is loaded.
            let cached_cover = abs_core::covers::cached_cover_path(&pool, session.server_id(), &item.item_id).await;
            {
                let mut inner = inner_rc.borrow_mut();
                if superseded(&inner) {
                    return;
                }
                if cached_cover.is_some() {
                    if let Some(pending) = &mut inner.pending_start {
                        pending.cover_path = cached_cover.clone();
                    }
                    inner.publish();
                }
            }
            // Captured up front — `item` and `session` move into `NowPlaying` below, and the
            // detached cover task needs these after that.
            let item_id = item.item_id.clone();
            let server_id = session.server_id().to_string();

            // Builds the "nothing could start" `now_playing` shared by every early-failure branch
            // below: `now_playing` is always constructed, even on failure, so the mini bar/Full
            // Player have something to show (with the error) instead of quietly never appearing.
            // Owns its own clones so it doesn't hold a borrow across the rest of this future.
            let failed_now_playing = {
                let item_id = item_id.clone();
                let server_id = server_id.clone();
                let session = session.clone();
                let title = item.title.clone();
                let author = item.author.clone();
                let cached_cover = cached_cover.clone();
                let retry_request = (item.clone(), default_speed);
                move |kind: abs_player::PlaybackErrorKind, message: String| NowPlaying {
                    item_id: item_id.clone(),
                    server_id: server_id.clone(),
                    account_id: session.account_id().to_string(),
                    session: session.clone(),
                    title: title.clone(),
                    author: author.clone(),
                    duration_seconds: 0.0,
                    tracks: Vec::new(),
                    current_track: 0,
                    current_source_is_local: false,
                    is_playing: false,
                    chapters: Vec::new(),
                    speed: 1.0,
                    sleep_timer: SleepTimerState::Off,
                    cover_path: cached_cover.clone(),
                    last_error: Some(abs_player::PlaybackError { kind, message, debug: None }),
                    last_known_within_track: 0.0,
                    seek_target_pending: false,
                    // `false`: with an empty `tracks` there is nothing to reload. `play()` sees
                    // `retry_request` instead and re-runs the whole start.
                    needs_reload: false,
                    seek_issued_at: None,
                    seek_retries: 0,
                    retry_request: Some(retry_request.clone()),
                    premature_eos_at: None,
                    loading_track: None,
                    ended: false,
                speed_unapplied: None,
                    seek_reload_used: false,
                }
            };
            let fail_start = {
                let inner_rc = inner_rc.clone();
                move |now_playing: NowPlaying| {
                    let mut inner = inner_rc.borrow_mut();
                    if !inner.pending_is(generation) {
                        tracing::info!(item_id = %now_playing.item_id, session = generation, "discarded a superseded start's failure");
                        return;
                    }
                    inner.reset_backend();
                    inner.pending_start = None;
                    inner.now_playing = Some(now_playing);
                    inner.publish();
                }
            };

            // Asked at resolve time, not captured earlier — see `NowPlaying::session`. The
            // connection (settings + resolved base URL) is asked the same way: a settings
            // change is honored by the very next playback without any rebuild.
            //
            // The device comes first: whether the track to start in is downloaded is decided from
            // the stored settings and token alone (no reachability probe, no token refresh), so a
            // downloaded book never waits on the network to find out it doesn't need it. A local
            // file needs no token, and a streamed track gets a current one when it loads
            // (`resolve_playable_url`). On the phone, an expired token on a poor connection used
            // to hold this up for ~40 s before the downloaded chapter was even looked at.
            let offline_mode = session.is_offline();
            let local_connection = match session.local_connection_target().await {
                Ok(connection) => connection,
                Err(err) => {
                    tracing::warn!(%err, item_id = %item.item_id, "couldn't load the server's connection settings");
                    fail_start(failed_now_playing(abs_player::PlaybackErrorKind::Network, err.to_string()));
                    return;
                }
            };
            let start_chapter_asked = inner_rc.borrow().pending_start.as_ref().and_then(|p| p.start_chapter);
            let mut files_only = start_target_from_files(
                &pool,
                &session,
                &item.item_id,
                start_chapter_asked,
                &local_connection,
                &session.stored_access_token(),
            )
            .await;
            if superseded(&inner_rc.borrow()) {
                return;
            }

            // Resolving the stream URL is required to proceed — unless the item can be played
            // from the files on the device instead. Reconciling progress is a nice-to-have that
            // must never add its own delay on top — both run concurrently, so a slow or
            // unreachable server is only ever felt once, not twice.
            //
            // When the track to start in is on the device, the server isn't worth waiting for:
            // with offline mode on it isn't asked at all, otherwise only for
            // `LOCAL_START_SERVER_WAIT` — the token, the connection probe and both calls
            // together (a reachable server answers well within that, and then still provides
            // fresh metadata and progress from other devices).
            let server_calls = |session: abs_core::auth::Session| {
                let pool = pool.clone();
                let item_id = item.item_id.clone();
                async move {
                    // Detached: a refresh the start stops waiting for still finishes and stores the
                    // rotated tokens, instead of being dropped after the server may have rotated them.
                    let access_token = {
                        let session = session.clone();
                        tokio::spawn(async move { session.access_token().await })
                    };
                    let connection = session.connection_target().await?;
                    let access_token = access_token.await.unwrap_or_else(|_| session.stored_access_token());
                    let answer = tokio::join!(
                        abs_core::streaming::resolve_stream_target(&connection, &access_token, &item_id),
                        abs_core::progress_sync::reconcile_item_progress(&pool, &connection, &access_token, session.account_id(), session.server_id(), &item_id),
                    );
                    Ok::<_, abs_core::CoreError>((connection, access_token, answer))
                }
            };
            let from_the_device = || (local_connection.clone(), session.stored_access_token(), None);
            let (connection, access_token, answer) = match (&files_only, offline_mode) {
                (Some(_), true) => {
                    tracing::info!(item_id = %item.item_id, "offline mode is on and the book is on the device; starting from the downloaded files without contacting the server");
                    from_the_device()
                }
                (Some(_), false) => match tokio::time::timeout(LOCAL_START_SERVER_WAIT, server_calls(session.clone())).await {
                    Ok(Ok((connection, access_token, answer))) => (connection, access_token, Some(answer)),
                    Ok(Err(err)) => {
                        tracing::warn!(%err, item_id = %item.item_id, "couldn't load the server's connection settings; starting from the downloaded files");
                        from_the_device()
                    }
                    Err(_) => {
                        tracing::info!(item_id = %item.item_id, wait_ms = LOCAL_START_SERVER_WAIT.as_millis() as u64, "the server is slow or unreachable; starting from the downloaded files");
                        from_the_device()
                    }
                },
                (None, false) => match server_calls(session.clone()).await {
                    Ok((connection, access_token, answer)) => (connection, access_token, Some(answer)),
                    Err(err) => {
                        tracing::warn!(%err, item_id = %item.item_id, "couldn't load the server's connection settings");
                        fail_start(failed_now_playing(abs_player::PlaybackErrorKind::Network, err.to_string()));
                        return;
                    }
                },
                (None, true) => {
                    tracing::info!(item_id = %item.item_id, "offline mode is on and the track to start in isn't downloaded; not starting");
                    fail_start(failed_now_playing(
                        abs_player::PlaybackErrorKind::Offline,
                        "the part of the book to start in isn't downloaded".to_string(),
                    ));
                    return;
                }
            };
            let (target_result, reconcile_result) = match answer {
                Some(answer) => answer,
                None => (Ok(files_only.take().expect("answer is None only when the files can start the book")), Ok(())),
            };
            if superseded(&inner_rc.borrow()) {
                return;
            }
            if let Err(err) = reconcile_result {
                tracing::warn!(%err, item_id = %item.item_id, "couldn't reconcile progress with the server; using local progress");
            }
            let target = match target_result {
                Ok(target) => target,
                // Nothing here to play (a podcast, an e-book): no download can stand in for it.
                Err(err @ abs_core::CoreError::NoPlayableAudio(_)) => {
                    tracing::warn!(%err, item_id = %item.item_id, "this item has no audio to play");
                    fail_start(failed_now_playing(abs_player::PlaybackErrorKind::NoAudio, err.to_string()));
                    return;
                }
                Err(err) => {
                    // The server is unreachable (or errored): fall back to locally cached track
                    // metadata, letting downloaded files play offline. A fully-downloaded item
                    // plays end-to-end; a partially-downloaded one starts and stops cleanly at
                    // the first gap (`spawn_load_track`'s load failure path); an item never
                    // resolved on this device has nothing to fall back on and can't start.
                    match abs_core::streaming::offline_stream_target(&pool, session.server_id(), &item.item_id, &connection, &access_token).await {
                        Ok(offline) => {
                            tracing::info!(item_id = %item.item_id, "couldn't reach the server; playing from locally cached tracks");
                            offline
                        }
                        Err(offline_err) => {
                            // An expired session is the most common trigger in practice
                            // (`resolve_stream_target` distinguishes a 401/403 as `CoreError::Auth`).
                            tracing::warn!(%err, offline = %offline_err, item_id = %item.item_id, "couldn't resolve a playable URL");
                            let kind = match err {
                                abs_core::CoreError::Auth => abs_player::PlaybackErrorKind::NotAuthorized,
                                abs_core::CoreError::Unreachable(_) => abs_player::PlaybackErrorKind::Unreachable,
                                _ => abs_player::PlaybackErrorKind::Network,
                            };
                            fail_start(failed_now_playing(kind, err.to_string()));
                            return;
                        }
                    }
                }
            };

            // A tapped chapter wins over the saved position. Read now rather than captured: the
            // same book started again while this one resolves can change it.
            let start_chapter = inner_rc.borrow().pending_start.as_ref().and_then(|p| p.start_chapter);
            let chapter_start = start_chapter.and_then(|index| target.chapters.get(index)).map(|c| c.start_seconds.max(0.0));
            // Reads whatever `reconcile_item_progress` just wrote, if it succeeded — falling back
            // to this client's own last local write (or nothing) otherwise. Book-level, so it can
            // fall in any track of a multi-file item.
            let saved = abs_storage::repo::progress::get(&pool, session.account_id(), session.server_id(), &item.item_id).await.ok().flatten();
            if superseded(&inner_rc.borrow()) {
                return;
            }
            let resume_at = match chapter_start {
                Some(at) => Some(at).filter(|at| *at > 0.0),
                None => saved.and_then(|p| resume_position(p.current_time_seconds, p.is_finished, target.duration_seconds)),
            };

            // A book-level resume position maps to (track, within-track) — the right file is the
            // one that gets loaded, rather than always starting from the first and trusting the
            // position to be inside it.
            let (start_track, start_within) =
                resume_at.map(|at| locate_track(&target.tracks, at)).unwrap_or((0, 0.0));

            // Local DB write only, no network involved. (Deliberately NOT syncing tracks here:
            // `upsert_all` deletes and re-inserts the item's track rows, and `download_tracks`
            // cascades on that FK, so a playback-time sync would wipe existing download state.)
            if let Err(err) = abs_core::chapters::sync_item_chapters(&pool, session.server_id(), &item.item_id, &target.chapters).await {
                tracing::warn!(%err, item_id = %item.item_id, "couldn't persist chapters locally");
            }
            let chapters = target
                .chapters
                .iter()
                .map(|c| ChapterInfo { title: c.title.clone(), start_seconds: c.start_seconds, end_seconds: c.end_seconds })
                .collect();

            let (start_url, start_is_local) =
                resolve_playable_url(&pool, &connection, session.server_id(), &item.item_id, &target.tracks[start_track].ino, &session).await;
            let downloaded_tracks =
                abs_core::download_tracks::complete_inos_for_item(&pool, session.server_id(), &item.item_id).await.map(|inos| inos.len()).unwrap_or(0);
            tracing::info!(
                item_id = %item.item_id,
                tracks = target.tracks.len(),
                downloaded_tracks,
                offline_mode,
                start_track,
                start_track_local = start_is_local,
                "track sources"
            );
            tracing::info!(
                item_id = %item.item_id,
                resume_at = resume_at.unwrap_or(0.0),
                ?start_chapter,
                track = start_track,
                within = start_within,
                tracks = target.tracks.len(),
                duration = target.duration_seconds,
                local = start_is_local,
                "resolved; loading"
            );

            let load = {
                let mut inner = inner_rc.borrow_mut();
                if superseded(&inner) {
                    return;
                }
                // As in `spawn_load_track`: transport properties go on before the load, so the
                // HTTP source created during it is set up with the connection's settings.
                inner.backend.apply_connection(&playback_properties(&connection));
                let load_generation = inner.begin_load();
                inner.backend.load(&start_url).map(|()| load_generation)
            };

            // Everything needed to show this item (title/author/cover/duration/chapters) is
            // already resolved above regardless of whether the load below succeeds — so
            // `now_playing` is always constructed, even on a load failure (e.g. no GStreamer
            // plugins for this format, no PulseAudio/PipeWire), with `last_error` saying why.
            let (loaded, applied_speed, last_error) = match load {
                Ok(load_generation) => {
                    // A seek needs the pipeline to have actually *reached* PAUSED, not just been
                    // asked to — for a network-streamed source (connecting, buffering) that can
                    // take a real moment, and a seek asked for before then silently does nothing.
                    let _ = inner_rc.borrow_mut().backend.pause();
                    // A non-default speed is itself a seek-with-rate, with the same requirement.
                    let needs_seek_ready = start_within > 0.0 || (default_speed - 1.0).abs() > f64::EPSILON;
                    if needs_seek_ready {
                        // `position()` only starts returning a value once preroll has genuinely
                        // completed (unlike `duration()`, which container metadata alone can
                        // answer), so it's the accurate "ready to seek" signal.
                        for _ in 0..PREROLL_WAIT_ATTEMPTS {
                            {
                                let inner = inner_rc.borrow();
                                if superseded(&inner) || inner.load_generation != load_generation {
                                    return;
                                }
                                if inner.backend.position().is_some() {
                                    break;
                                }
                            }
                            glib::timeout_future(Duration::from_millis(100)).await;
                        }
                    }

                    let mut inner = inner_rc.borrow_mut();
                    if superseded(&inner) || inner.load_generation != load_generation {
                        return;
                    }
                    // One call for position and rate together (see `AudioBackend::set_speed`).
                    let applied_speed = if (default_speed - 1.0).abs() > f64::EPSILON {
                        if inner.backend.set_speed(default_speed, Duration::from_secs_f64(start_within)).is_ok() { default_speed } else { 1.0 }
                    } else {
                        if start_within > 0.0 {
                            let _ = inner.backend.seek(Duration::from_secs_f64(start_within));
                        }
                        1.0
                    };
                    (true, applied_speed, None)
                }
                Err(err) => {
                    tracing::warn!(%err, item_id = %item.item_id, "couldn't load the audio stream");
                    let mut inner = inner_rc.borrow_mut();
                    if superseded(&inner) {
                        return;
                    }
                    // Nothing was successfully set up on the backend — release it rather than
                    // leave a half-loaded pipeline sitting on the audio server.
                    inner.reset_backend();
                    (false, 1.0, Some(abs_player::PlaybackError::from(&err)))
                }
            };

            let mut inner = inner_rc.borrow_mut();
            // Play only if nothing paused it while it resolved: the user, an unplug, a call.
            let pending = inner.pending_start.take();
            let wants_play = pending.as_ref().is_some_and(|p| p.wants_play);
            let intent = pending.and_then(|p| p.intent);
            let is_playing = loaded && wants_play;
            if is_playing {
                let _ = inner.backend.play();
            }
            tracing::info!(item_id = %item.item_id, loaded, playing = is_playing, speed = applied_speed, "playback ready");
            inner.now_playing = Some(NowPlaying {
                item_id: item.item_id,
                server_id: session.server_id().to_string(),
                account_id: session.account_id().to_string(),
                session,
                title: item.title,
                author: item.author,
                duration_seconds: target.duration_seconds,
                tracks: target.tracks,
                current_track: start_track,
                current_source_is_local: start_is_local,
                is_playing,
                chapters,
                speed: applied_speed,
                sleep_timer: SleepTimerState::Off,
                cover_path: cached_cover,
                last_error,
                last_known_within_track: start_within,
                // The readiness wait above gives up after 15 s on a slow stream, and a seek asked
                // for before the pipeline can take it silently does nothing — playback would then
                // run from the track's start and save that over the real position. The resume
                // seek is only trusted once `observe_position` sees it land.
                seek_target_pending: loaded && start_within > 0.0,
                seek_issued_at: (loaded && start_within > 0.0).then(Instant::now),
                seek_retries: 0,
                retry_request: None,
                premature_eos_at: None,
                // A load failure leaves nothing loaded, so the first Retry reloads.
                needs_reload: !loaded,
                loading_track: None,
                ended: false,
                speed_unapplied: None,
                seek_reload_used: false,
            });
            inner.last_progress_write = Instant::now();
            inner.last_played_at = Instant::now();
            inner.publish();
            drop(inner);
            if is_playing {
                controller.ensure_ticking();
            }
            // Asked for during the start: the book is loaded now, so it is a plain action on it.
            match intent {
                Some(StartIntent::Reset) => {
                    controller.reset_progress();
                }
                Some(StartIntent::MarkFinished) => {
                    controller.mark_as_finished();
                }
                None => {}
            }

            // The cover download, spawned only now that it can't race `now_playing` into
            // existence, and never gating the first note. It only ever *replaces* the cached
            // cover the snapshot was seeded with: a failure changes nothing, and the same path
            // again (a cache hit) isn't republished. Not fetched at all in offline mode.
            if !offline_mode {
                let inner_rc = inner_rc.clone();
                let pool = pool.clone();
                let paths = paths.clone();
                let connection = connection.clone();
                let access_token = access_token.clone();
                glib::spawn_future_local(async move {
                    let Some(cover_path) =
                        abs_core::covers::fetch_and_cache_cover(&paths, &pool, &connection, &access_token, &server_id, &item_id).await
                    else {
                        return;
                    };
                    let mut inner = inner_rc.borrow_mut();
                    if inner.session_generation != generation {
                        return;
                    }
                    match &mut inner.now_playing {
                        Some(now_playing)
                            if now_playing.item_id == item_id && now_playing.cover_path.as_deref() != Some(cover_path.as_path()) =>
                        {
                            now_playing.cover_path = Some(cover_path);
                        }
                        _ => return,
                    }
                    inner.publish();
                });
            }
        });
    }

    pub fn play(&self) {
        // Whatever paused it, it is being resumed now: a pause mark left behind (the user pressed
        // Play before the replug, say) would let a later replug or call ending "resume" a
        // different, deliberate pause.
        {
            let mut inner = self.inner.borrow_mut();
            inner.paused_by_unplug = false;
            inner.paused_by_call = false;
        }
        // A book still resolving plays once it's loaded.
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(pending) = &mut inner.pending_start {
                pending.wants_play = true;
                tracing::info!(item_id = %pending.item_id, "play: will start once loaded");
                inner.publish();
                return;
            }
        }
        // A start that failed before anything resolved has nothing loaded to resume — run the
        // start again (see `NowPlaying::retry_request`).
        let restart = self.inner.borrow().now_playing.as_ref().and_then(|np| {
            let (request, default_speed) = np.retry_request.clone()?;
            Some((np.session.clone(), request, default_speed))
        });
        if let Some((session, request, default_speed)) = restart {
            tracing::info!(item_id = %request.item_id, "play: retrying a start that failed");
            self.start(session, request, default_speed);
            return;
        }
        // A finished book starts over. Resuming the pipeline where it stopped would only reach
        // its end-of-stream again at once; a fresh load of the first file can't (the reset in
        // `prepare_track_load` drops whatever the old one left on the bus).
        {
            let mut inner = self.inner.borrow_mut();
            let ended = inner.now_playing.as_ref().is_some_and(|np| np.ended && !np.tracks.is_empty());
            if ended {
                let Some(now_playing) = &mut inner.now_playing else { return };
                tracing::info!(item_id = %now_playing.item_id, "finished book restarted");
                now_playing.ended = false;
                now_playing.is_playing = true;
                now_playing.last_error = None;
                let item_id = now_playing.item_id.clone();
                inner.paused_by_unplug = false;
                inner.paused_by_call = false;
                let generation = inner.prepare_track_load(0, 0.0);
                inner.write_progress_at(0.0, false, true);
                inner.last_played_at = Instant::now();
                inner.publish();
                drop(inner);
                self.ensure_ticking();
                if let Some(generation) = generation {
                    Inner::spawn_load_track(self.inner.clone(), item_id, generation);
                }
                return;
            }
        }
        self.reconcile_before_resuming();
        let mut inner = self.inner.borrow_mut();
        inner.prepare_to_resume();
        let position = inner.book_position();
        let Some(now_playing) = &mut inner.now_playing else { return };
        tracing::info!(item_id = %now_playing.item_id, position, "play");
        // A track load in flight applies `is_playing` itself once the file is ready.
        if now_playing.loading_track.is_some() {
            now_playing.is_playing = true;
            inner.publish();
            drop(inner);
            self.ensure_ticking();
            return;
        }
        // The backend was released (`AudioBackend::reset`) after a stream error or a failed
        // load left nothing loaded to simply resume — GStreamer never recovers a pipeline from
        // an error state without a fresh `load()`. Reload the current track from the last
        // known-good position instead, the same path a cross-track seek already uses. Every
        // resume path (this method) — the banner's Retry, MPRIS `Play`/`PlayPause`, a headphone
        // replug, a plain re-tap of the play button — funnels through here, so all of them
        // recover the same way, rather than each needing its own reload logic.
        if now_playing.needs_reload {
            now_playing.is_playing = true;
            now_playing.last_error = None;
            let item_id = now_playing.item_id.clone();
            let track_index = now_playing.current_track;
            let within = now_playing.last_known_within_track;
            tracing::info!(%item_id, track = track_index, within, "reloading the track to resume");
            let generation = inner.prepare_track_load(track_index, within);
            inner.publish();
            drop(inner);
            self.ensure_ticking();
            if let Some(generation) = generation {
                Inner::spawn_load_track(self.inner.clone(), item_id, generation);
            }
            return;
        }
        inner.apply_deferred_speed();
        let started = match inner.backend.play() {
            Ok(()) => {
                if let Some(now_playing) = &mut inner.now_playing {
                    now_playing.is_playing = true;
                    now_playing.last_error = None;
                }
                true
            }
            // A synchronous failure here means the pipeline itself is broken — this is the retry
            // path a banner's "Retry" action and a plain re-tap of Play both go through, so it
            // must also (re-)populate `last_error` rather than silently doing nothing.
            Err(err) => {
                tracing::warn!(%err, "backend.play() failed; resetting the pipeline and marking it for reload");
                if let Some(now_playing) = &mut inner.now_playing {
                    now_playing.last_error = Some((&err).into());
                    now_playing.needs_reload = true;
                }
                inner.reset_backend();
                false
            }
        };
        inner.publish();
        drop(inner);
        if started {
            self.ensure_ticking();
        }
    }

    /// When playback resumes after a long pause, another device may have moved this book on
    /// meanwhile (the mini bar keeps a paused book loaded for days). Resuming here without
    /// checking would carry on from the old position and push it over the newer one. So playback
    /// starts right away, and in parallel the server's copy is fetched: if it is elsewhere and
    /// isn't simply this device's own last write, playback jumps there and `on_position_adopted`
    /// is told, so the shell can offer Undo. Position writes are held until the check resolves.
    ///
    /// The local row may already hold the newer server value before this runs — Home and Library
    /// pull progress from the server on their own — so what counts is whether the reconciled value
    /// is this device's own, not whether it changed during this check.
    fn reconcile_before_resuming(&self) {
        let (pool, session, account_id, server_id, item_id, from, generation, seek_count) = {
            let mut inner = self.inner.borrow_mut();
            let due = inner.last_played_at.elapsed() >= inner.resume_reconcile_after;
            let from = inner.book_position();
            let generation = inner.session_generation;
            let seek_count = inner.seek_count;
            let Some(now_playing) = &inner.now_playing else { return };
            if now_playing.session.is_offline() {
                return;
            }
            if now_playing.is_playing || now_playing.tracks.is_empty() || !due || inner.holding_writes_for_reconcile.is_some() {
                return;
            }
            let context = (
                inner.pool.clone(),
                now_playing.session.clone(),
                now_playing.account_id.clone(),
                now_playing.server_id.clone(),
                now_playing.item_id.clone(),
                from,
                generation,
                seek_count,
            );
            inner.holding_writes_for_reconcile = Some(generation);
            context
        };
        let controller = self.clone();
        glib::spawn_future_local(async move {
            let reconciled = match session.connection_target().await {
                Ok(connection) => {
                    let access_token = session.access_token().await;
                    abs_core::progress_sync::reconcile_item_progress(&pool, &connection, &access_token, &account_id, &server_id, &item_id).await
                }
                Err(err) => Err(err),
            };
            let after = abs_storage::repo::progress::get(&pool, &account_id, &server_id, &item_id).await.ok().flatten();
            if let Err(err) = &reconciled {
                tracing::info!(%err, item_id = %item_id, "couldn't check the server for newer progress before resuming");
            }
            let (still_current, seeked_since, own_write) = {
                let mut inner = controller.inner.borrow_mut();
                if inner.holding_writes_for_reconcile == Some(generation) {
                    inner.holding_writes_for_reconcile = None;
                }
                let still_current = inner.session_generation == generation && inner.now_playing.as_ref().is_some_and(|np| np.item_id == item_id);
                let own_write = after.as_ref().is_some_and(|a| inner.progress_writer.is_written_locally(&item_id, a.current_time_seconds, a.is_finished));
                (still_current, inner.seek_count != seek_count, own_write)
            };
            if !still_current {
                return;
            }
            let adopt_to = match (&reconciled, &after) {
                (Ok(()), Some(after))
                    if !after.is_finished && !own_write && !seeked_since && (after.current_time_seconds - from).abs() > ADOPT_SERVER_POSITION_MIN_DELTA_SECONDS =>
                {
                    Some(after.current_time_seconds)
                }
                _ => None,
            };
            match adopt_to {
                Some(to) => {
                    tracing::info!(item_id = %item_id, from, to, "resuming from newer progress made on another device");
                    controller.seek_to_seconds(to);
                    let on_position_adopted = controller.inner.borrow().on_position_adopted.clone();
                    if let Some(on_position_adopted) = on_position_adopted {
                        on_position_adopted(from, to);
                    }
                }
                // Whatever was held back meanwhile (a pause) is written now.
                None => {
                    let paused = controller.inner.borrow().now_playing.as_ref().is_some_and(|np| !np.is_playing);
                    if paused {
                        controller.inner.borrow_mut().write_progress();
                    }
                }
            }
        });
    }

    /// Registers the listener told `(from, to)` when resuming jumped to newer progress from
    /// another device — the shell toasts it with an Undo.
    pub fn set_on_position_adopted(&self, listener: impl Fn(f64, f64) + 'static) {
        self.inner.borrow_mut().on_position_adopted = Some(Rc::new(listener));
    }

    /// Writes the current position now and pushes it even if the server already had it — for
    /// Undo after adopting another device's position.
    pub fn save_progress_now(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.progress_writer.forget_pushed();
        inner.write_progress();
    }

    /// A seek while paused used to be saved only at the next play/pause, so skipping to where
    /// the listener actually was and then quitting lost that correction. Saved (and pushed) once
    /// the seeking stops for `PAUSED_SEEK_WRITE_DELAY`; a seek while playing is picked up by the
    /// tick's own writes.
    fn save_paused_seek_later(&self) {
        let generation = {
            let mut inner = self.inner.borrow_mut();
            if inner.now_playing.as_ref().is_none_or(|np| np.is_playing) {
                return;
            }
            inner.paused_seek_generation += 1;
            inner.paused_seek_generation
        };
        let inner = Rc::downgrade(&self.inner);
        glib::timeout_add_local_once(PAUSED_SEEK_WRITE_DELAY, move || {
            let Some(inner) = inner.upgrade() else { return };
            let mut inner = inner.borrow_mut();
            let still_paused = inner.now_playing.as_ref().is_some_and(|np| !np.is_playing);
            if inner.paused_seek_generation == generation && still_paused {
                inner.write_progress();
            }
        });
    }

    /// Writes the final position and waits (briefly) for it to land — for app shutdown, where
    /// otherwise up to `LOCAL_PROGRESS_WRITE_INTERVAL` of listening (and anything still queued)
    /// would be lost.
    pub fn flush_on_shutdown(&self) {
        let writer = {
            let mut inner = self.inner.borrow_mut();
            inner.write_progress_now();
            inner.progress_writer.clone()
        };
        writer.drain_blocking(SHUTDOWN_PUSH_TIMEOUT);
    }

    /// A handle that doesn't keep the controller alive — for the app's shutdown hook, which
    /// outlives any one shell.
    pub fn downgrade(&self) -> WeakPlayerController {
        WeakPlayerController { inner: Rc::downgrade(&self.inner), tick_source: Rc::downgrade(&self.tick_source) }
    }

    #[cfg(test)]
    pub fn set_resume_reconcile_after(&self, after: Duration) {
        self.inner.borrow_mut().resume_reconcile_after = after;
    }

    #[cfg(test)]
    pub fn set_stale_connection_after(&self, after: Duration) {
        self.inner.borrow_mut().stale_connection_after = after;
    }

    /// As if a stream error had just been recovered from by itself: the next one stops with the
    /// error, for tests of what the listener sees then.
    #[cfg(test)]
    pub fn use_up_auto_recovery(&self) {
        self.inner.borrow_mut().last_auto_recover_at = Some(Instant::now());
    }

    pub fn pause(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.paused_by_unplug = false;
        inner.paused_by_call = false;
        // A book still resolving stays paused once it's loaded.
        if let Some(pending) = &mut inner.pending_start {
            pending.wants_play = false;
            tracing::info!(item_id = %pending.item_id, "pause: will stay paused once loaded");
            inner.publish();
            return;
        }
        let position = inner.book_position();
        if let Some(now_playing) = &inner.now_playing {
            tracing::info!(item_id = %now_playing.item_id, position, "pause");
        }
        // Nothing is loaded to pause once `needs_reload` is set (the backend was already
        // released), nor while a track load is in flight — just record the intent; the next
        // `play()` goes through the reload path, and a load applies `is_playing` itself.
        if inner.now_playing.as_ref().is_some_and(|np| np.needs_reload || np.loading_track.is_some()) {
            if let Some(now_playing) = &mut inner.now_playing {
                now_playing.is_playing = false;
            }
        } else {
            match inner.backend.pause() {
                Ok(()) => {
                    if let Some(now_playing) = &mut inner.now_playing {
                        now_playing.is_playing = false;
                    }
                    // `Ok` here only means GStreamer accepted the request, not that the pipeline
                    // has actually reached `Paused` — see `AudioBackend::pause`'s doc comment.
                    // Confirms it did, or recovers if it never does.
                    Inner::spawn_pause_confirmation(self.inner.clone(), inner.load_generation, inner.seek_count);
                }
                Err(err) => {
                    tracing::warn!(%err, "backend.pause() failed; resetting the pipeline and marking it for reload");
                    // The backend is reset below regardless of what set_state's actual failure
                    // was — nothing is left running to call this "still playing". Leaving
                    // `is_playing` at its old value here (a bug this fixes) meant a failed pause
                    // published a snapshot that still claimed to be playing — indistinguishable
                    // from a working pause to both the UI and to `handle_route_event`'s own
                    // `is_playing` check on any *next* route event, even though the pipeline had
                    // just been torn down to `Null`.
                    if let Some(now_playing) = &mut inner.now_playing {
                        now_playing.is_playing = false;
                        now_playing.last_error = Some((&err).into());
                        now_playing.needs_reload = true;
                    }
                    inner.reset_backend();
                }
            }
        }
        inner.publish();
        inner.write_progress();
    }

    /// For when connectivity returns while paused/idle — the only place progress otherwise syncs
    /// is the periodic tick (only while playing) and `pause()`/`start()`, none of which fire on
    /// their own just because the network came back. Reconciles first (in case another device
    /// moved this item's progress further while this device was offline) and only pushes this
    /// device's local position if reconciling did NOT just overwrite it with something newer from
    /// the server — pushing unconditionally would silently undo that correction. A no-op if
    /// nothing is loaded.
    pub fn sync_pending_progress(&self) {
        let Some((pool, account_id, server_id, item_id, session, generation)) = ({
            let inner = self.inner.borrow();
            inner.now_playing.as_ref().map(|np| {
                (inner.pool.clone(), np.account_id.clone(), np.server_id.clone(), np.item_id.clone(), np.session.clone(), inner.session_generation)
            })
        }) else {
            return;
        };
        if session.is_offline() {
            return;
        }
        let inner_rc = self.inner.clone();
        glib::spawn_future_local(async move {
            let before = abs_storage::repo::progress::get(&pool, &account_id, &server_id, &item_id).await.ok().flatten();
            let connection = match session.connection_target().await {
                Ok(connection) => connection,
                Err(err) => {
                    tracing::info!(%err, item_id = %item_id, "couldn't load connection settings; still offline");
                    return;
                }
            };
            let access_token = session.access_token().await;
            if let Err(err) =
                abs_core::progress_sync::reconcile_item_progress(&pool, &connection, &access_token, &account_id, &server_id, &item_id).await
            {
                tracing::info!(%err, item_id = %item_id, "couldn't reconcile progress on reconnect; still offline");
                return;
            }
            let after = abs_storage::repo::progress::get(&pool, &account_id, &server_id, &item_id).await.ok().flatten();
            let server_had_something_newer = match (&before, &after) {
                (Some(b), Some(a)) => a.updated_at > b.updated_at,
                (None, Some(_)) => true,
                _ => false,
            };
            if server_had_something_newer {
                return;
            }
            // The awaits above took a while: only the book this started for gets written.
            let mut inner = inner_rc.borrow_mut();
            if inner.session_generation == generation && inner.now_playing.as_ref().is_some_and(|np| np.item_id == item_id) {
                inner.write_progress();
            }
        });
    }

    /// Applies Settings → Playback's headphone behavior switches (persisted; the live controller
    /// must follow immediately, not on the next app start).
    pub fn set_headphone_behavior(&self, pause_on_unplug: bool, resume_on_replug: bool) {
        let mut inner = self.inner.borrow_mut();
        inner.pause_on_unplug = pause_on_unplug;
        inner.resume_on_replug = resume_on_replug;
    }

    /// What `set_headphone_behavior` last applied — for asserting that the Settings screen's
    /// switches actually reach the controller.
    #[cfg(test)]
    pub fn headphone_behavior(&self) -> (bool, bool) {
        let inner = self.inner.borrow();
        (inner.pause_on_unplug, inner.resume_on_replug)
    }

    /// Applies Settings → Playback's "Buffer streams in bursts" switch (persisted; the live
    /// controller must follow immediately, not on the next app start, same as
    /// `set_headphone_behavior`). Takes effect on the *next* track load — an already-loaded
    /// pipeline's fetch strategy doesn't change retroactively (see `AudioBackend::
    /// set_burst_buffering`'s doc comment).
    /// Registers the one listener told how each background progress push to the server ended.
    /// The shell uses it to toast a sync failure once per failure episode.
    pub fn set_on_progress_sync(&self, listener: impl Fn(ProgressSyncOutcome) + 'static) {
        self.inner.borrow_mut().on_progress_sync = Some(Rc::new(listener));
    }

    pub fn set_burst_buffering(&self, enabled: bool) {
        let mut inner = self.inner.borrow_mut();
        inner.burst_buffering = enabled;
        inner.backend.set_burst_buffering(enabled);
    }

    /// What `set_burst_buffering` last applied — for asserting that the Settings screen's switch
    /// actually reaches the controller.
    #[cfg(test)]
    pub fn burst_buffering(&self) -> bool {
        self.inner.borrow().burst_buffering
    }

    /// Applies Settings → Playback's start-of-session speed and skip intervals (persisted; the
    /// live controller must follow immediately, not on the next app start). Same story as
    /// `set_headphone_behavior` — the consumers that used to capture these values at build time
    /// (the play closure in `main_window`, the player screen's skip buttons and actions,
    /// `MprisBridge`) read the getters below at call time instead.
    pub fn set_playback_config(&self, default_speed: f64, skip_back_seconds: f64, skip_forward_seconds: f64) {
        let mut inner = self.inner.borrow_mut();
        inner.default_speed = usable_speed(default_speed).unwrap_or(abs_core::playback::DEFAULT_SPEED);
        inner.skip_back_seconds = skip_back_seconds;
        inner.skip_forward_seconds = skip_forward_seconds;
    }

    /// The speed every `start()` begins a session at — Settings → Playback's "Default speed".
    pub fn default_speed(&self) -> f64 {
        self.inner.borrow().default_speed
    }

    /// The skip intervals as `(back, forward)` seconds — Settings → Playback's skip rows.
    pub fn skip_intervals(&self) -> (f64, f64) {
        let inner = self.inner.borrow();
        (inner.skip_back_seconds, inner.skip_forward_seconds)
    }

    /// Reacts to `abs_player::route_watch` events: an unplug pauses (when enabled, and only if
    /// something is actually playing — that is the pause a replug may later lift), a replug
    /// resumes **only** that kind of pause, and only when enabled. A manual pause, phone call,
    /// sleep timer or end-of-book pause clears the "paused by unplug" mark (in `pause()` and
    /// `tick()`'s own pause paths), so none of them can ever be overridden by a reconnection.
    pub fn handle_route_event(&self, event: abs_player::route_watch::RouteEvent) {
        // Every branch logs its decision, including every reason for doing nothing — this is
        // the only place that can tell "the setting is off", "nothing is playing" and "nothing
        // is loaded" apart on a device that isn't pausing, once `route_watch`'s own "headphone
        // route changed" line has confirmed the event was even delivered.
        let seq = {
            let mut inner = self.inner.borrow_mut();
            inner.route_event_seq += 1;
            inner.route_event_seq
        };
        match event {
            abs_player::route_watch::RouteEvent::Unplugged => {
                let (pause_on_unplug, intent) = {
                    let inner = self.inner.borrow();
                    (inner.pause_on_unplug, inner.playing_intent())
                };
                let has_now_playing = intent.is_some();
                let is_playing = intent == Some(true);
                if !pause_on_unplug {
                    tracing::info!("headphone unplug ignored: \"pause when headphones disconnect\" is off");
                    return;
                }
                if !has_now_playing {
                    tracing::info!("headphone unplug ignored: nothing is loaded");
                    return;
                }
                if !is_playing {
                    tracing::info!("headphone unplug ignored: already paused");
                    return;
                }
                // `pause()` clears the mark (it's the generic pause path — also used for calls,
                // MPRIS and the user); the unplug then re-marks it as *its* pause.
                self.pause();
                {
                    let mut inner = self.inner.borrow_mut();
                    inner.paused_by_unplug = true;
                    inner.unplug_paused_at = Some(Instant::now());
                }
                tracing::info!("paused for headphone unplug");
            }
            abs_player::route_watch::RouteEvent::Replugged => {
                let (resume_on_replug, paused_by_unplug, is_paused) = {
                    let inner = self.inner.borrow();
                    (inner.resume_on_replug, inner.paused_by_unplug, inner.playing_intent() == Some(false))
                };
                if !resume_on_replug {
                    tracing::info!("headphone replug ignored: \"resume when headphones reconnect\" is off");
                    return;
                }
                if !paused_by_unplug {
                    tracing::info!("headphone replug ignored: the current pause (if any) wasn't caused by an unplug");
                    return;
                }
                if !is_paused {
                    tracing::info!("headphone replug ignored: not paused");
                    return;
                }
                // Resumes once the plug has held for `REPLUG_SETTLE`.
                let controller = self.downgrade();
                glib::timeout_add_local_once(REPLUG_SETTLE, move || {
                    let Some(controller) = controller.upgrade() else { return };
                    let still_pending = {
                        let inner = controller.inner.borrow();
                        inner.route_event_seq == seq && inner.paused_by_unplug && inner.playing_intent() == Some(false)
                    };
                    if !still_pending {
                        tracing::info!("headphone replug ignored: the jack bounced, or something else resumed or paused meanwhile");
                        return;
                    }
                    controller.inner.borrow_mut().paused_by_unplug = false;
                    controller.play();
                    tracing::info!("resumed for headphone replug");
                });
            }
        }
    }

    /// Reacts to `abs_player::call_watch` events: a call starting to ring (or dial) pauses —
    /// right away, not only once it's picked up; an incoming call that ends without ever being
    /// answered (rejected, missed, the caller gave up) resumes, but **only** if that call's own
    /// pause is still the reason playback is paused; a call that was answered, or one this phone
    /// placed, never resumes anything — the user picks the book back up themselves once they're
    /// off the phone.
    pub fn handle_call_event(&self, event: abs_player::call_watch::CallEvent) {
        // Every branch logs its decision, like `handle_route_event` — `call_watch`'s own "phone
        // call event" line confirms the event was delivered, this says what came of it.
        use abs_player::call_watch::CallEvent;
        match event {
            CallEvent::Started { incoming } => {
                let kind = if incoming { "incoming" } else { "outgoing" };
                let intent = self.inner.borrow().playing_intent();
                let (has_now_playing, is_playing) = (intent.is_some(), intent == Some(true));
                if !has_now_playing {
                    tracing::info!(kind, "phone call ignored: nothing is loaded");
                    return;
                }
                if !is_playing {
                    tracing::info!(kind, "phone call ignored: already paused");
                    return;
                }
                // `pause()` clears the mark (it's the generic pause path); the call then
                // re-marks it as *its* pause.
                self.pause();
                self.inner.borrow_mut().paused_by_call = true;
                tracing::info!(kind, "paused for phone call");
            }
            CallEvent::Answered => {
                let was_marked = std::mem::replace(&mut self.inner.borrow_mut().paused_by_call, false);
                if was_marked {
                    tracing::info!("phone call answered; playback will stay paused after it ends");
                } else {
                    tracing::info!("phone call answered");
                }
            }
            CallEvent::Ended { answered, outgoing } => {
                let (paused_by_call, is_paused) = {
                    let mut inner = self.inner.borrow_mut();
                    let is_paused = inner.playing_intent() == Some(false);
                    (std::mem::replace(&mut inner.paused_by_call, false), is_paused)
                };
                if answered {
                    tracing::info!("phone call ended after being answered; not resuming");
                    return;
                }
                if outgoing {
                    tracing::info!("outgoing phone call ended; not resuming");
                    return;
                }
                if !paused_by_call {
                    tracing::info!("unanswered phone call ended; not resuming: the current pause (if any) wasn't caused by the call");
                    return;
                }
                if !is_paused {
                    tracing::info!("unanswered phone call ended; not resuming: not paused");
                    return;
                }
                self.play();
                tracing::info!("resumed after an unanswered phone call");
            }
        }
    }

    /// MPRIS's `PlayPause`, as opposed to the app's own play/pause buttons (which call
    /// `toggle_play_pause` directly — a tap on the screen is always deliberate).
    ///
    /// Pulling a TRRS headset's plug (one with an inline button or mic) drags the contacts past
    /// the button ring, which the kernel reports as a headset Play/Pause key press; the desktop's
    /// media-key handling forwards it to the active player as exactly this call. It lands within
    /// milliseconds of the unplug — on the Librem 5 field report, ~2ms after
    /// `handle_route_event` had paused — and, being a *toggle*, it resumed the playback the unplug
    /// had just paused, through the phone's speaker. So a `PlayPause` arriving within
    /// [`SPURIOUS_UNPLUG_TOGGLE_WINDOW`] of an unplug pause is ignored. The window is far shorter
    /// than any deliberate reaction (the same report's real press on the lock screen came ~3s
    /// later), and it only applies while that unplug pause is still the reason playback is
    /// paused — any other pause path clears `paused_by_unplug`, so a manual pause followed by a
    /// quick `PlayPause` still toggles as usual.
    pub fn external_play_pause(&self) {
        let since_unplug_pause = {
            let inner = self.inner.borrow();
            let paused = inner.playing_intent() == Some(false);
            if paused && inner.paused_by_unplug {
                inner.unplug_paused_at.map(|at| at.elapsed())
            } else {
                None
            }
        };
        if let Some(elapsed) = since_unplug_pause.filter(|elapsed| *elapsed < SPURIOUS_UNPLUG_TOGGLE_WINDOW) {
            tracing::info!(
                elapsed_ms = elapsed.as_millis() as u64,
                "ignored MPRIS PlayPause right after a headphone-unplug pause (a headset button contact shorting as the plug is pulled)"
            );
            return;
        }
        self.toggle_play_pause();
    }

    pub fn toggle_play_pause(&self) {
        // The intent, not what's audible: a book still loading counts as playing if it will
        // play once loaded, so the button that shows "pause" pauses it.
        let is_playing = self.inner.borrow().playing_intent().unwrap_or(false);
        if is_playing {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn skip(&self, delta_seconds: f64) {
        let target = {
            let inner = self.inner.borrow();
            let Some(now_playing) = &inner.now_playing else { return };
            let current = inner.book_position();
            clamp_to_book(current + delta_seconds, now_playing.duration_seconds)
        };
        self.seek_to_seconds(target);
    }

    /// Seeks to the start of the next chapter. Returns `false` (and does nothing) when nothing is
    /// loaded, the book has no chapters, or it's already in the last one.
    pub fn next_chapter(&self) -> bool {
        self.seek_to_chapter_start(next_chapter_start)
    }

    /// Seeks back to the start of the current chapter, or to the previous chapter's start when
    /// just past the current one's — see `previous_chapter_start`. Returns `false` (and does
    /// nothing) when nothing is loaded or the book has no chapters.
    pub fn previous_chapter(&self) -> bool {
        self.seek_to_chapter_start(previous_chapter_start)
    }

    fn seek_to_chapter_start(&self, pick: fn(&[ChapterInfo], f64) -> Option<f64>) -> bool {
        let target = {
            let inner = self.inner.borrow();
            let Some(now_playing) = &inner.now_playing else { return false };
            pick(&now_playing.chapters, inner.book_position())
        };
        let Some(target) = target else { return false };
        self.seek_to_seconds(target);
        true
    }

    /// Seeks to an absolute book-level position — the primitive behind the scrubber, `skip`,
    /// the chapters sheet (tap-to-seek to a chapter's start) and MPRIS `Seek`/`SetPosition`. A
    /// position inside a different file than the one loaded (or past its end) is a cross-track
    /// seek: the state machine switches to the target track and `spawn_load_track` brings the
    /// actual pipeline there asynchronously.
    pub fn seek_to_seconds(&self, seconds: f64) {
        let plan = {
            let mut inner = self.inner.borrow_mut();
            let from = inner.book_position();
            let load_generation = inner.load_generation;
            let Some(now_playing) = &mut inner.now_playing else { return };
            // A start that failed before anything resolved has nothing to seek in.
            if now_playing.tracks.is_empty() {
                tracing::info!(item_id = %now_playing.item_id, "seek ignored: nothing is loaded");
                return;
            }
            let target = clamp_to_book(seconds, now_playing.duration_seconds);
            // Moving anywhere in a finished book takes it back to unfinished, there.
            now_playing.ended = false;
            // A seek the listener asked for gets its own chance to land (see `observe_position`).
            now_playing.seek_reload_used = false;
            let (track_index, within) = locate_track(&now_playing.tracks, target);
            let cross_track = track_index != now_playing.current_track;
            tracing::info!(item_id = %now_playing.item_id, from, to = target, track = track_index, within, cross_track, "seek");
            // An end-of-chapter sleep timer follows the listener to the chapter they're now in.
            if let SleepTimerState::Armed(SleepTimerDeadline::Position(_)) = now_playing.sleep_timer {
                let end = chapter_end_at(&now_playing.chapters, target).unwrap_or(now_playing.duration_seconds);
                now_playing.sleep_timer = SleepTimerState::Armed(SleepTimerDeadline::Position(end));
            }
            // Set before the seek/reload below even starts — see `book_position`'s doc comment:
            // a `FLUSH` seek on a streamed file has no reported position until the new HTTP
            // response arrives (seconds, or forever on a dead connection), and without this,
            // every tick/progress-write in that window would misread the position as "start of
            // the current track" rather than the position just requested.
            now_playing.last_known_within_track = within;
            let plan = if !cross_track {
                // Marks the position as unconfirmed until `observe_position` sees the backend
                // actually agree with `within` — covers both a real network seek's stall and
                // (for the non-local branch below) the brief async gap before that seek is even
                // issued.
                now_playing.seek_target_pending = true;
                now_playing.seek_retries = 0;
                if now_playing.loading_track.is_some() || now_playing.needs_reload {
                    // Nothing usable is loaded: the load in flight (or the next play's reload)
                    // starts at `last_known_within_track`, which now holds this target.
                    now_playing.seek_issued_at = None;
                    None
                } else if now_playing.current_source_is_local {
                    now_playing.seek_issued_at = Some(Instant::now());
                    inner.drop_stale_end_of_stream();
                    let _ = inner.backend.seek(Duration::from_secs_f64(within));
                    None
                } else {
                    // The seek is issued at once — it used to wait on the check below, a read
                    // that queues behind whatever else holds the database pool. A download may
                    // have completed since this track started streaming, which the check
                    // resolves just below, switching to the file if so (the seek then lands in
                    // it instead).
                    now_playing.seek_issued_at = Some(Instant::now());
                    inner.drop_stale_end_of_stream();
                    let _ = inner.backend.seek(Duration::from_secs_f64(within));
                    let now_playing = inner.now_playing.as_ref().expect("checked above");
                    Some(SeekPlan::MaybeLocalNow {
                        item_id: now_playing.item_id.clone(),
                        server_id: now_playing.server_id.clone(),
                        track_index,
                        ino: now_playing.tracks.get(track_index).map(|t| t.ino.clone()),
                        load_generation,
                    })
                }
            } else {
                let item_id = now_playing.item_id.clone();
                inner.prepare_track_load(track_index, within).map(|generation| SeekPlan::CrossTrack { item_id, generation })
            };
            inner.seek_count += 1;
            inner.publish();
            plan
        };
        self.save_paused_seek_later();
        match plan {
            None => {}
            Some(SeekPlan::CrossTrack { item_id, generation }) => {
                Inner::spawn_load_track(self.inner.clone(), item_id, generation);
            }
            Some(SeekPlan::MaybeLocalNow { item_id, server_id, track_index, ino, load_generation }) => {
                let inner_rc = self.inner.clone();
                glib::spawn_future_local(async move {
                    let pool = inner_rc.borrow().pool.clone();
                    let has_local = match &ino {
                        Some(ino) => abs_core::download_tracks::local_track_path(&pool, &server_id, &item_id, ino).await.is_some(),
                        None => false,
                    };
                    let mut inner = inner_rc.borrow_mut();
                    // Still the same pipeline, book and track this seek was for — anything that
                    // loaded or released the backend meanwhile makes this seek stale, and a stale
                    // seek must never land on a pipeline that has since moved on.
                    let still_current = inner.load_generation == load_generation
                        && inner.now_playing.as_ref().is_some_and(|np| np.item_id == item_id && np.current_track == track_index);
                    if !still_current {
                        return;
                    }
                    // The newest target, not this seek's own: a later seek may have been asked
                    // for while this check ran.
                    let Some(within) = inner.now_playing.as_ref().map(|np| np.last_known_within_track) else { return };
                    if has_local {
                        tracing::info!(%item_id, track = track_index, "a download finished since this track started streaming; switching to it");
                        if let Some(generation) = inner.prepare_track_load(track_index, within) {
                            drop(inner);
                            Inner::spawn_load_track(inner_rc, item_id, generation);
                        }
                    }
                });
            }
        }
    }

    /// Changes the playback rate. Only updates the reported speed if the backend actually
    /// accepted it, matching `play()`/`pause()`'s existing "state reflects reality" pattern.
    ///
    /// The backend is handed the position this controller knows to be right (a pending seek's
    /// target included) rather than asked for its own, which can be stale or missing right
    /// after a seek. With no usable pipeline (a track load in flight, or released after an
    /// error), only the speed is recorded; the load applies it.
    pub fn set_speed(&self, speed: f64) {
        let Some(speed) = usable_speed(speed) else {
            tracing::warn!(speed, "ignored an unusable playback speed");
            return;
        };
        let mut inner = self.inner.borrow_mut();
        let holds_track = inner.backend_holds_track();
        let Some(now_playing) = &inner.now_playing else { return };
        if (now_playing.speed - speed).abs() < f64::EPSILON {
            return;
        }
        let within = if now_playing.seek_target_pending {
            now_playing.last_known_within_track
        } else {
            inner.backend.position().map(|d| d.as_secs_f64()).unwrap_or(now_playing.last_known_within_track)
        };
        tracing::info!(item_id = %now_playing.item_id, speed, "speed");
        // Paused on a stream: record the speed and let Play apply it. Applying it now is a
        // flushing seek — a range request that stalls on a bad connection — for nothing audible.
        if holds_track && !now_playing.is_playing && !now_playing.current_source_is_local {
            if let Some(now_playing) = &mut inner.now_playing {
                let applied = *now_playing.speed_unapplied.get_or_insert(now_playing.speed);
                now_playing.speed = speed;
                // Back at what the backend runs at: nothing left to apply.
                if (applied - speed).abs() < f64::EPSILON {
                    now_playing.speed_unapplied = None;
                }
            }
            inner.publish();
            return;
        }
        // A rate change is a seek to the pipeline: a pause confirmation in flight must not read
        // the preroll it causes as a pause that never landed.
        inner.seek_count += 1;
        if holds_track {
            inner.drop_stale_end_of_stream();
        }
        let accepted = !holds_track || inner.backend.set_speed(speed, Duration::from_secs_f64(within)).is_ok();
        if accepted {
            if let Some(now_playing) = &mut inner.now_playing {
                now_playing.speed = speed;
                now_playing.speed_unapplied = None;
            }
        }
        inner.publish();
    }

    /// Arms a wall-clock sleep timer: playback pauses once `minutes` have passed, checked once
    /// per tick rather than via a second timer source (see `SleepTimerDeadline`).
    pub fn set_sleep_timer_minutes(&self, minutes: u32) {
        let armed = {
            let mut inner = self.inner.borrow_mut();
            let deadline = SleepTimerDeadline::WallClock(Instant::now() + Duration::from_secs(u64::from(minutes) * 60));
            let armed = inner.now_playing.is_some();
            if let Some(now_playing) = &mut inner.now_playing {
                now_playing.sleep_timer = SleepTimerState::Armed(deadline);
            }
            inner.publish();
            armed
        };
        // A wall-clock deadline must keep counting down even if the player is currently paused
        // (or gets paused a moment later) — `tick` is the only thing that checks it, so the timer
        // has to be running for as long as this stays armed, independent of `is_playing`.
        if armed {
            self.ensure_ticking();
        }
    }

    /// Arms a sleep timer that fires at the end of whatever chapter is currently playing, falling
    /// back to the end of the item if there's no chapter data (or the position doesn't fall
    /// inside any known chapter).
    pub fn set_sleep_timer_end_of_chapter(&self) {
        let mut inner = self.inner.borrow_mut();
        let position = inner.book_position();
        let Some(now_playing) = &mut inner.now_playing else { return };
        if now_playing.tracks.is_empty() {
            return;
        }
        let end = chapter_end_at(&now_playing.chapters, position).unwrap_or(now_playing.duration_seconds);
        now_playing.sleep_timer = SleepTimerState::Armed(SleepTimerDeadline::Position(end));
        inner.publish();
    }

    pub fn cancel_sleep_timer(&self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(now_playing) = &mut inner.now_playing {
            now_playing.sleep_timer = SleepTimerState::Off;
        }
        inner.publish();
    }

    /// Runs one tick's worth of work (bus events, sleep-timer deadline, snapshot publish, the
    /// periodic progress write) and reports whether the tick timer should keep firing.
    /// `ensure_ticking`'s closure turns this straight into a `glib::ControlFlow` — see its doc
    /// comment for the install half of this. Ticking is only worth doing while something is
    /// actually playing (there's nothing to report otherwise) or a wall-clock sleep-timer
    /// deadline is still counting down (the one thing that must keep advancing even while
    /// paused); once neither holds, this returns `false` and the caller lets the source
    /// self-destroy rather than leaving a 250ms timer running against a player with nothing to
    /// do — see `tick_source`'s doc comment for why that used to be the case and cost real
    /// battery.
    fn tick(&self) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.now_playing.is_none() {
            return false;
        }

        if let Some((item_id, generation)) = inner.observe_position() {
            Inner::spawn_load_track(self.inner.clone(), item_id, generation);
        }

        let event = match inner.deferred_event.take() {
            Some(event) => Some(event),
            None => inner.backend.poll_event(),
        };
        if let Some(event) = event {
            let verdict = match event {
                abs_player::PlayerEvent::EndOfStream => inner.judge_end_of_stream(),
                abs_player::PlayerEvent::Error(_) => EndOfStreamVerdict::RealEnd,
            };
            if let EndOfStreamVerdict::Unclear { within, short_by } = verdict {
                // Possibly the end, possibly a stream cut short near it: stop without marking the
                // book finished. Play reloads here — more audio if there is some, or the same end
                // again, which then counts as the real one.
                tracing::warn!(within, short_by, "the last file ended before its reported length; pausing without marking the book finished");
                inner.reset_backend();
                if let Some(now_playing) = &mut inner.now_playing {
                    now_playing.is_playing = false;
                    now_playing.needs_reload = true;
                    now_playing.last_known_within_track = within;
                    now_playing.seek_target_pending = false;
                }
                inner.paused_by_unplug = false;
                inner.paused_by_call = false;
                inner.write_progress();
                inner.publish();
                // Only a wall-clock sleep timer still needs the tick while paused.
                return matches!(
                    inner.now_playing.as_ref().map(|n| n.sleep_timer),
                    Some(SleepTimerState::Armed(SleepTimerDeadline::WallClock(_)))
                );
            }
            let event = match event {
                abs_player::PlayerEvent::EndOfStream if verdict == EndOfStreamVerdict::Premature => {
                    tracing::warn!("the stream ended well before the end of the track; treating it as a lost connection");
                    abs_player::PlayerEvent::Error(abs_player::PlaybackError {
                        kind: abs_player::PlaybackErrorKind::Network,
                        message: "The audio stream ended before the end of the file.".to_string(),
                        debug: None,
                    })
                }
                event => event,
            };
            match event {
                abs_player::PlayerEvent::EndOfStream => {
                    if let Some((item_id, next_track)) = inner.next_track_after_end_of_stream() {
                        // The next file exists — keep going. The load happens on the main loop
                        // (see `spawn_load_track`); the state machine has already moved on.
                        tracing::info!(%item_id, track = next_track, "file ended; moving on to the next one");
                        if let Some(generation) = inner.prepare_track_load(next_track, 0.0) {
                            Inner::spawn_load_track(self.inner.clone(), item_id, generation);
                        }
                        // `spawn_load_track` hasn't run `backend.load()` yet — the backend still
                        // reports the *old* pipeline's position, which `book_position()` would
                        // now misattribute to the new (already-bumped) `current_track`'s offset,
                        // producing a bogus overshot position (old-track offset's worth of extra
                        // seconds) for one tick. Returning here instead of falling through to the
                        // unconditional `publish()` below skips that one bad snapshot; the reload
                        // publishes its own correct one (`backend_pos: None` right after `load()`)
                        // moments later. Caught via a real timing-dependent test failure once an
                        // async DB check (`resolve_playable_url`) widened this race's window
                        // enough to make it land inside a test's polling loop. Still playing (the
                        // next file is about to load) — keep the timer running.
                        return true;
                    } else {
                        tracing::info!("reached the end of the book");
                        inner.pause_backend_if_loaded();
                        if let Some(now_playing) = &mut inner.now_playing {
                            now_playing.is_playing = false;
                            now_playing.ended = true;
                        }
                        // End-of-book is not an unplug pause — a replug must not revive it
                        // (nor a call ending).
                        inner.paused_by_unplug = false;
                        inner.paused_by_call = false;
                        inner.write_progress_now();
                    }
                }
                abs_player::PlayerEvent::Error(err) => {
                    tracing::warn!(?err, "playback error");
                    // The pipeline's `pulsesink` stream and HTTP connection are released right
                    // away, never left open on a pipeline GStreamer cannot recover without a
                    // fresh `load()` — see `AudioBackend::reset`'s doc comment and this plan's
                    // Librem 5 field report (a broken pipeline was previously left holding the
                    // audio device across every failed Retry).
                    let position = inner.book_position();
                    inner.reset_backend();
                    let load_generation = inner.load_generation;

                    // Everything a possible recovery needs, captured while `now_playing` is
                    // still borrowed — `err` travels inside `Recovery` rather than as a bare
                    // variable used again after this match, specifically so its single move (in
                    // whichever branch below) is visible to the borrow checker as tied to
                    // `recovery` itself, not to two separate, correlated-only-by-logic uses.
                    // `None` overall means there's nothing loaded to recover (shouldn't happen —
                    // this event only fires while something is — but is handled rather than
                    // assumed away).
                    struct Recovery {
                        item_id: String,
                        server_id: String,
                        track_index: usize,
                        within: f64,
                        ino: Option<String>,
                        err: abs_player::PlaybackError,
                    }
                    let recovery: Option<Recovery> = match &mut inner.now_playing {
                        None => None,
                        Some(now_playing) => {
                            now_playing.needs_reload = true;
                            let track_offset =
                                now_playing.tracks.get(now_playing.current_track).map(|t| t.offset_seconds).unwrap_or(0.0);
                            now_playing.last_known_within_track = (position - track_offset).max(0.0);
                            if now_playing.current_source_is_local {
                                // Already the best available source — nothing to fall back to.
                                now_playing.is_playing = false;
                                now_playing.last_error = Some(err);
                                None
                            } else {
                                // A download may have completed since this track started
                                // streaming — worth checking before showing an error the user
                                // can't do anything about (see `seek_to_seconds`'s twin check,
                                // and this plan's section E). `is_playing` is deliberately left
                                // untouched here: a successful reload below resumes seamlessly;
                                // only the async check's own failure branch turns this into a
                                // visible error.
                                Some(Recovery {
                                    item_id: now_playing.item_id.clone(),
                                    server_id: now_playing.server_id.clone(),
                                    track_index: now_playing.current_track,
                                    within: now_playing.last_known_within_track,
                                    ino: now_playing.tracks.get(now_playing.current_track).map(|t| t.ino.clone()),
                                    err,
                                })
                            }
                        }
                    };
                    inner.paused_by_unplug = false;
                    inner.paused_by_call = false;
                    inner.write_progress();

                    if let Some(recovery) = recovery {
                        let pool = inner.pool.clone();
                        let inner_rc = self.inner.clone();
                        glib::spawn_future_local(async move {
                            let has_local = match &recovery.ino {
                                Some(ino) => abs_core::download_tracks::local_track_path(&pool, &recovery.server_id, &recovery.item_id, ino)
                                    .await
                                    .is_some(),
                                None => false,
                            };
                            let mut inner = inner_rc.borrow_mut();
                            // Anything that loaded or released the backend since (a Retry, a
                            // seek, another book) has taken over from this recovery.
                            if inner.load_generation != load_generation
                                || inner.now_playing.as_ref().is_none_or(|np| np.item_id != recovery.item_id)
                            {
                                return;
                            }
                            // Once per `AUTO_RECOVER_WINDOW`, a failed stream is simply loaded
                            // again — a fresh URL, token and connection, which is all a dropped
                            // connection or an expired token needs, and all Retry would do.
                            let auto_reload = !has_local && inner.last_auto_recover_at.is_none_or(|at| at.elapsed() >= AUTO_RECOVER_WINDOW);
                            if has_local || auto_reload {
                                if has_local {
                                    tracing::info!(item_id = %recovery.item_id, track = recovery.track_index, "the stream failed but the file is downloaded now; switching to it");
                                } else {
                                    tracing::info!(item_id = %recovery.item_id, track = recovery.track_index, within = recovery.within, kind = ?recovery.err.kind, "stream error; reloading at the same position");
                                    inner.last_auto_recover_at = Some(Instant::now());
                                }
                                if let Some(generation) = inner.prepare_track_load(recovery.track_index, recovery.within) {
                                    drop(inner);
                                    Inner::spawn_load_track(inner_rc.clone(), recovery.item_id, generation);
                                    if auto_reload {
                                        Inner::log_if_the_stream_is_slow_to_answer(inner_rc, generation);
                                    }
                                }
                            } else {
                                if let Some(now_playing) = &mut inner.now_playing {
                                    now_playing.is_playing = false;
                                    now_playing.last_error = Some(recovery.err);
                                }
                                inner.publish();
                            }
                        });
                    }
                }
            }
        }

        if let Some(SleepTimerState::Armed(deadline)) = inner.now_playing.as_ref().map(|n| n.sleep_timer) {
            let reached = match deadline {
                SleepTimerDeadline::WallClock(at) => Instant::now() >= at,
                SleepTimerDeadline::Position(end) => inner.book_position() >= end,
            };
            if reached {
                tracing::info!("sleep timer reached; pausing");
                inner.pause_backend_if_loaded();
                if let Some(now_playing) = &mut inner.now_playing {
                    now_playing.is_playing = false;
                    now_playing.sleep_timer = SleepTimerState::Off;
                }
                // A sleep-timer pause is deliberate; a replug (or a call ending) must not
                // override it.
                inner.paused_by_unplug = false;
                inner.paused_by_call = false;
                inner.write_progress_now();
            }
        }

        inner.publish();

        let is_playing = inner.now_playing.as_ref().is_some_and(|n| n.is_playing);
        if is_playing {
            inner.last_played_at = Instant::now();
        }
        if is_playing && inner.holding_writes_for_reconcile.is_none() && inner.last_progress_write.elapsed() >= LOCAL_PROGRESS_WRITE_INTERVAL {
            // Not `write_progress`: this is the routine "still playing" heartbeat, not a
            // meaningful state change, so it must not force a server round trip on every firing —
            // `write_progress_at`'s own `force_server_sync: false` still syncs once
            // `SERVER_PROGRESS_SYNC_INTERVAL` has actually elapsed.
            let position = inner.book_position();
            inner.write_progress_at(position, false, false);
        }

        // Computed *after* the sleep-timer check above (which may have just turned an armed
        // wall-clock deadline back off) — otherwise a tick that reaches the deadline would keep
        // itself alive for one extra, pointless round based on the now-stale "was armed" state.
        let wall_clock_deadline_armed = matches!(
            inner.now_playing.as_ref().map(|n| n.sleep_timer),
            Some(SleepTimerState::Armed(SleepTimerDeadline::WallClock(_)))
        );

        is_playing || wall_clock_deadline_armed
    }
}

pub struct MiniPlayerBar {
    pub root: gtk4::Widget,
    pub controller: PlayerController,
    #[cfg(test)]
    pub hooks: MiniPlayerHooks,
}

#[cfg(test)]
pub struct MiniPlayerHooks {
    pub bar: gtk4::Box,
    pub card: gtk4::Box,
    pub cover_picture: gtk4::Picture,
    pub title_label: gtk4::Label,
    pub author_label: gtk4::Label,
    pub play_button: gtk4::Button,
    pub progress: gtk4::ProgressBar,
    pub error_icon: gtk4::Image,
}

/// The widgets shared by `build_mini_bar` (owns a freshly created `PlayerController`) and
/// `build_mini_bar_for` (binds to an already-live one) — everything about the mini bar except how
/// the resulting snapshot closure reaches its controller (constructor argument vs.
/// `PlayerController::add_listener`) is identical between the two.
struct MiniBarWidgets {
    bar: gtk4::Box,
    #[cfg(test)]
    card: gtk4::Box,
    cover: CoverImage,
    title_label: gtk4::Label,
    author_label: gtk4::Label,
    play_icon: gtk4::Image,
    play_button: gtk4::Button,
    progress: gtk4::ProgressBar,
    error_icon: gtk4::Image,
}

/// The mini bar's look, loaded once per process (same `Once`-guarded `CssProvider` idiom as
/// `screens::library::ensure_busy_overlay_css`). The strip under the card copies libadwaita's own
/// `actionbar` rule — the `AdwViewSwitcherBar` right below it is one — so bar and tab bar read as
/// one bottom area, set off from the scrolling content by the same top line; named colors only,
/// so dark and high-contrast follow by themselves.
fn ensure_mini_bar_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(
            "box.mini-player { background-color: @headerbar_bg_color; color: @headerbar_fg_color; \
                               box-shadow: inset 0 1px @headerbar_shade_color; } \
             box.mini-player:backdrop { background-color: @headerbar_backdrop_color; }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("a display for the app's css"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

/// From `docs/design/ui-spec.md`'s "Player — mini" section: cover placeholder, title/author,
/// play/pause, a thin progress line — hidden until something has actually played. Drawn as a
/// stock libadwaita `card` on a strip styled like the tab bar (see `ensure_mini_bar_css`).
fn build_mini_bar_widgets() -> MiniBarWidgets {
    ensure_mini_bar_css();
    let cover = CoverImage::new(40);

    let title_label = gtk4::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .build();
    let author_label = gtk4::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label"])
        .build();
    let text_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).hexpand(true).valign(gtk4::Align::Center).build();
    text_box.append(&title_label);
    text_box.append(&author_label);

    let play_icon = gtk4::Image::from_icon_name("media-playback-pause-symbolic");
    let play_button = gtk4::Button::builder().css_classes(["circular", "flat"]).child(&play_icon).build();

    // A persistent, lightweight indicator that the last playback attempt failed — see
    // `PlayerSnapshot::last_error`'s doc comment. There's no room in this compact bar for the
    // Full Player screen's full banner text, so this is deliberately just a glyph: enough for a
    // glance to know something needs attention, with "tap to open the full player" (already the
    // mini bar's own established gesture) as the way to actually see why.
    let error_icon = gtk4::Image::builder().icon_name("dialog-warning-symbolic").tooltip_text(tr("Playback error — tap for details")).visible(false).build();

    let content_row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(10)
        .margin_start(8)
        .margin_end(4)
        .margin_top(8)
        .build();
    content_row.append(cover.widget());
    content_row.append(&text_box);
    content_row.append(&error_icon);
    content_row.append(&play_button);

    // Inset from the card's rounded corners rather than running edge to edge.
    let progress = gtk4::ProgressBar::builder().margin_start(10).margin_end(10).margin_top(8).margin_bottom(10).build();

    // The margins leave room for the card's shadow, which would otherwise be clipped.
    let card = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .css_classes(["card"])
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    card.append(&content_row);
    card.append(&progress);

    let bar = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).css_classes(["mini-player"]).visible(false).build();
    bar.append(&card);

    MiniBarWidgets {
        bar,
        #[cfg(test)]
        card,
        cover,
        title_label,
        author_label,
        play_icon,
        play_button,
        progress,
        error_icon,
    }
}

/// The closure that applies a snapshot to `widgets` — shared between `build_mini_bar` (registered
/// via `PlayerController::new`) and `build_mini_bar_for` (registered via `add_listener`, and also
/// called once immediately to prime from whatever's already playing).
///
/// Returns `false` once the bar itself is gone (it is held weakly — the closure keeping its own
/// bar alive is what let every Item Detail visit's mini bar outlive its screen), which a scoped
/// listener takes as "stop calling me".
fn mini_bar_snapshot_applier(widgets: &MiniBarWidgets) -> impl Fn(&PlayerSnapshot) -> bool + 'static {
    let bar = widgets.bar.downgrade();
    let title_label = widgets.title_label.clone();
    let author_label = widgets.author_label.clone();
    let play_icon = widgets.play_icon.clone();
    let progress = widgets.progress.clone();
    let cover = widgets.cover.clone();
    let error_icon = widgets.error_icon.clone();
    move |snapshot: &PlayerSnapshot| {
        let Some(bar) = bar.upgrade() else { return false };
        bar.set_visible(true);
        title_label.set_label(&snapshot.title);
        author_label.set_label(snapshot.author.as_deref().unwrap_or(""));
        author_label.set_visible(snapshot.author.is_some());
        cover.set_path(snapshot.cover_path.as_deref());
        play_icon.set_icon_name(Some(if snapshot.shows_pause_button() {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        }));
        error_icon.set_visible(snapshot.last_error.is_some());
        let fraction = if snapshot.duration_seconds > 0.0 {
            (snapshot.position_seconds / snapshot.duration_seconds).clamp(0.0, 1.0)
        } else {
            0.0
        };
        progress.set_fraction(fraction);
        true
    }
}

fn mini_bar_from_widgets(widgets: MiniBarWidgets, controller: PlayerController) -> MiniPlayerBar {
    widgets.play_button.connect_clicked({
        let controller = controller.clone();
        move |_| controller.toggle_play_pause()
    });

    MiniPlayerBar {
        root: widgets.bar.clone().upcast(),
        controller,
        #[cfg(test)]
        hooks: MiniPlayerHooks {
            bar: widgets.bar,
            card: widgets.card,
            cover_picture: widgets.cover.picture().clone(),
            title_label: widgets.title_label,
            author_label: widgets.author_label,
            play_button: widgets.play_button,
            progress: widgets.progress,
            error_icon: widgets.error_icon,
        },
    }
}

pub fn build_mini_bar(pool: SqlitePool, paths: AppPaths, backend: Box<dyn abs_player::AudioBackend>) -> MiniPlayerBar {
    let widgets = build_mini_bar_widgets();
    let apply_snapshot = mini_bar_snapshot_applier(&widgets);
    let controller = PlayerController::new(pool, paths, backend, move |snapshot| {
        apply_snapshot(snapshot);
    });
    mini_bar_from_widgets(widgets, controller)
}

/// Same mini-bar widget, bound to an already-live `PlayerController` via `add_listener` instead of
/// owning construction — used by screens (Item Detail) that need to reflect the exact same
/// playback state the shell's own mini bar shows, without creating a second, independent
/// controller. Primes immediately from `controller.snapshot()`: `add_listener` only fires on the
/// *next* published snapshot, and unlike the shell's own bar (always built before anything can be
/// playing) this can be built well after playback already started.
pub fn build_mini_bar_for(controller: PlayerController) -> MiniPlayerBar {
    let widgets = build_mini_bar_widgets();
    let apply_snapshot = mini_bar_snapshot_applier(&widgets);
    if let Some(snapshot) = controller.snapshot() {
        apply_snapshot(&snapshot);
    }
    controller.add_scoped_listener(apply_snapshot);
    mini_bar_from_widgets(widgets, controller)
}

/// Implements `abs_player::mpris::MprisCommands` by calling back into a real `PlayerController`
/// — this is the whole dependency-direction resolution for MPRIS: `abs-player`'s `mpris` module
/// only ever calls this trait, never anything from `app` directly.
pub struct MprisBridge {
    controller: PlayerController,
}

impl MprisBridge {
    pub fn new(controller: PlayerController) -> Self {
        Self { controller }
    }
}

impl abs_player::mpris::MprisCommands for MprisBridge {
    fn play_pause(&self) {
        self.controller.external_play_pause();
    }
    fn play(&self) {
        self.controller.play();
    }
    fn pause(&self) {
        self.controller.pause();
    }
    fn seek(&self, offset_micros: i64) {
        self.controller.skip(offset_micros as f64 / 1_000_000.0);
    }
    fn set_position(&self, position_micros: i64) {
        self.controller.seek_to_seconds(position_micros as f64 / 1_000_000.0);
    }
    // The card's ⏮/⏭ change chapters; its own seek buttons already cover skipping. A book
    // without chapters falls back to the skip intervals so the buttons still do something.
    fn next(&self) {
        if !self.controller.has_chapters() {
            self.controller.skip(self.controller.skip_intervals().1);
        } else {
            self.controller.next_chapter();
        }
    }
    fn previous(&self) {
        if !self.controller.has_chapters() {
            self.controller.skip(-self.controller.skip_intervals().0);
        } else {
            self.controller.previous_chapter();
        }
    }
}

/// Converts a snapshot into MPRIS's own state shape — the one place `PlayerSnapshot`'s fields get
/// translated into `xesam:*`/`mpris:*` terms. `mpris:artUrl` needs a `file://` URI, not a plain
/// path, hence `gio::File::for_path(..).uri()`.
pub fn mpris_state_from_snapshot(snapshot: &PlayerSnapshot) -> abs_player::mpris::PlayerState {
    abs_player::mpris::PlayerState {
        status: if snapshot.is_playing { abs_player::mpris::PlaybackStatus::Playing } else { abs_player::mpris::PlaybackStatus::Paused },
        metadata: abs_player::mpris::TrackMetadata {
            title: snapshot.title.clone(),
            artist: snapshot.author.clone(),
            length_micros: (snapshot.duration_seconds * 1_000_000.0) as i64,
            art_url: snapshot.cover_path.as_deref().map(|path| gio::File::for_path(path).uri().to_string()),
        },
        position_micros: (snapshot.position_seconds * 1_000_000.0) as i64,
        rate: snapshot.speed,
    }
}

/// A no-op backend used when `GstBackend::new()` fails (e.g. no GStreamer plugins available) —
/// matches `main.rs`'s existing tolerance for a GStreamer init failure (it warns and continues
/// rather than crashing the whole app over unavailable audio). The mini-player bar and full
/// player screen still build normally; every playback action just logs and does nothing.
struct NullBackend;

impl abs_player::AudioBackend for NullBackend {
    fn load(&mut self, _uri: &str) -> abs_player::Result<()> {
        Err(abs_player::PlayerError::NoSourceLoaded)
    }
    fn apply_connection(&mut self, _properties: &abs_player::ConnectionProperties) {
        // Nothing to configure: there is no transport behind this backend at all.
    }
    fn play(&mut self) -> abs_player::Result<()> {
        Err(abs_player::PlayerError::NoSourceLoaded)
    }
    fn pause(&mut self) -> abs_player::Result<()> {
        Err(abs_player::PlayerError::NoSourceLoaded)
    }
    fn is_paused(&self) -> bool {
        // `pause()` above always errors — this is never consulted in practice, but the trait
        // still needs an answer: nothing is ever loaded, so nothing is ever paused either.
        false
    }
    fn seek(&mut self, _position: Duration) -> abs_player::Result<()> {
        Err(abs_player::PlayerError::NoSourceLoaded)
    }
    fn set_speed(&mut self, _speed: f64, _position: Duration) -> abs_player::Result<()> {
        Err(abs_player::PlayerError::NoSourceLoaded)
    }
    fn position(&self) -> Option<Duration> {
        None
    }
    fn duration(&self) -> Option<Duration> {
        None
    }
    fn poll_event(&self) -> Option<abs_player::PlayerEvent> {
        None
    }
    fn set_burst_buffering(&mut self, _enabled: bool) {
        // Nothing to configure: there is no transport behind this backend at all.
    }
    fn reset(&mut self) {
        // Nothing to release: there is no transport behind this backend at all.
    }
}

/// The real, production audio backend — `GstBackend::new()` (the system default
/// `autoaudiosink`), falling back to `NullBackend` if GStreamer can't build a pipeline at all.
pub fn real_backend() -> Box<dyn abs_player::AudioBackend> {
    match abs_player::GstBackend::new() {
        Ok(backend) => Box::new(backend),
        Err(err) => {
            tracing::warn!(%err, "couldn't build the audio backend; playback will be unavailable");
            Box::new(NullBackend)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::{pool, pump_until};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// A real, valid PNG (1x1, black pixel) — cover endpoints serve real images, and downloaded
    /// bodies are validated with the `image` crate before being cached, so fake byte vectors
    /// would (correctly) be rejected. Same fixture the `CoverImage` and `covers` tests use.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63,
        0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
        0x42, 0x60, 0x82,
    ];

    /// A real, valid WAV file's bytes — served over HTTP by wiremock so `PlayerController::start`
    /// exercises the actual network fetch (via GStreamer's own HTTP source), not a `file://` URI.
    /// Same fixture trick `abs-player`'s own tests use, just written to an in-memory buffer
    /// instead of disk since this is served, not loaded from a path.
    fn silent_wav_bytes(seconds: u32) -> Vec<u8> {
        let spec = hound::WavSpec { channels: 1, sample_rate: 8000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut buf, spec).unwrap();
            for _ in 0..(spec.sample_rate * seconds) {
                writer.write_sample(0i16).unwrap();
            }
            writer.finalize().unwrap();
        }
        buf.into_inner()
    }

    /// `fakesink` for the audio output (no real hardware in a sandbox), but a real GStreamer
    /// pipeline and a real HTTP fetch otherwise — network liveness and audio-hardware presence
    /// are separate concerns.
    pub(crate) fn test_backend() -> Box<dyn abs_player::AudioBackend> {
        abs_player::init().expect("gstreamer should initialize in this environment");
        Box::new(abs_player::GstBackend::new_with_sink("fakesink").expect("build a playbin with a fake sink"))
    }

    /// A backend that fails every operation with `NoSourceLoaded` — the same shape as
    /// `crate::player::real_backend`'s private `NullBackend` fallback (used when `GstBackend::new`
    /// fails entirely, e.g. no GStreamer plugins at all), reproduced here since that type isn't
    /// exported: this crate has no seam to make a *real* `GstBackend` fail synchronously (its
    /// `load()` only ever fails via the async bus, not its `Result`), so this is the only way to
    /// exercise `start()`'s "the audio engine itself is unavailable" branch deterministically.
    #[derive(Default)]
    struct FailingBackend {
        /// How many times `reset()` was called — asserted by the "a failed load/state change
        /// always releases the backend" regression tests, since a real `GstBackend` has no seam
        /// to check this against a synchronously-failing `set_state`.
        reset_calls: Rc<std::cell::Cell<u32>>,
    }
    impl abs_player::AudioBackend for FailingBackend {
        fn load(&mut self, _uri: &str) -> abs_player::Result<()> {
            Err(abs_player::PlayerError::NoSourceLoaded)
        }
        fn apply_connection(&mut self, _properties: &abs_player::ConnectionProperties) {}
        fn play(&mut self) -> abs_player::Result<()> {
            Err(abs_player::PlayerError::NoSourceLoaded)
        }
        fn pause(&mut self) -> abs_player::Result<()> {
            Err(abs_player::PlayerError::NoSourceLoaded)
        }
        fn is_paused(&self) -> bool {
            false
        }
        fn seek(&mut self, _position: Duration) -> abs_player::Result<()> {
            Err(abs_player::PlayerError::NoSourceLoaded)
        }
        fn set_speed(&mut self, _speed: f64, _position: Duration) -> abs_player::Result<()> {
            Err(abs_player::PlayerError::NoSourceLoaded)
        }
        fn position(&self) -> Option<Duration> {
            None
        }
        fn duration(&self) -> Option<Duration> {
            None
        }
        fn poll_event(&self) -> Option<abs_player::PlayerEvent> {
            None
        }
        fn set_burst_buffering(&mut self, _enabled: bool) {}
        fn reset(&mut self) {
            self.reset_calls.set(self.reset_calls.get() + 1);
        }
    }

    /// Responds to a `Range: bytes=START-[END]` request with `206 Partial Content` and the
    /// matching slice, like the real Audiobookshelf server does (confirmed live: its file
    /// endpoint sends `accept-ranges: bytes`) — GStreamer's `souphttpsrc` issues a byte-range
    /// request for every seek, including the one `PlayerController::start` performs to resume
    /// mid-book, so a mock that ignores `Range` and always replies with the full body from byte 0
    /// would make every resume-from-progress test pass or fail for the wrong reason.
    fn ranged_response(body: Vec<u8>) -> impl Fn(&Request) -> ResponseTemplate + Send + Sync {
        move |req: &Request| {
            let Some(range) = req.headers.get("Range").and_then(|v| v.to_str().ok()) else {
                return ResponseTemplate::new(200).insert_header("Accept-Ranges", "bytes").set_body_bytes(body.clone());
            };
            let Some(spec) = range.strip_prefix("bytes=") else {
                return ResponseTemplate::new(200).insert_header("Accept-Ranges", "bytes").set_body_bytes(body.clone());
            };
            let (start_str, end_str) = spec.split_once('-').unwrap_or((spec, ""));
            let start: usize = start_str.parse().unwrap_or(0);
            let end = if end_str.is_empty() { body.len() - 1 } else { end_str.parse().unwrap_or(body.len() - 1) };
            let end = end.min(body.len() - 1);
            let slice = body[start..=end].to_vec();
            ResponseTemplate::new(206)
                .insert_header("Content-Range", format!("bytes {start}-{end}/{}", body.len()))
                .insert_header("Accept-Ranges", "bytes")
                .set_body_bytes(slice)
        }
    }

    /// Like `ranged_response`, but any range request that does *not* start at byte 0 — i.e. a
    /// seek's own follow-up request, never the initial from-the-top load — is held for `delay`
    /// before responding. Models a stalled/flaky connection specifically at the moment of a seek,
    /// which is exactly what `souphttpsrc` does on a `FLUSH` seek: issue a brand-new range
    /// request and report no position until its response arrives (see this plan's Librem 5
    /// field report and `Inner::book_position`'s doc comment).
    fn ranged_response_delayed_when_seeking(body: Vec<u8>, delay: Duration) -> impl Fn(&Request) -> ResponseTemplate + Send + Sync {
        move |req: &Request| {
            let Some(range) = req.headers.get("Range").and_then(|v| v.to_str().ok()) else {
                return ResponseTemplate::new(200).insert_header("Accept-Ranges", "bytes").set_body_bytes(body.clone());
            };
            let Some(spec) = range.strip_prefix("bytes=") else {
                return ResponseTemplate::new(200).insert_header("Accept-Ranges", "bytes").set_body_bytes(body.clone());
            };
            let (start_str, end_str) = spec.split_once('-').unwrap_or((spec, ""));
            let start: usize = start_str.parse().unwrap_or(0);
            let end = if end_str.is_empty() { body.len() - 1 } else { end_str.parse().unwrap_or(body.len() - 1) };
            let end = end.min(body.len() - 1);
            let slice = body[start..=end].to_vec();
            let response = ResponseTemplate::new(206)
                .insert_header("Content-Range", format!("bytes {start}-{end}/{}", body.len()))
                .insert_header("Accept-Ranges", "bytes")
                .set_body_bytes(slice);
            if start > 0 { response.set_delay(delay) } else { response }
        }
    }

    pub(crate) async fn mock_playable_item(mock_server: &MockServer, item_id: &str, seconds: u32) {
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [{ "ino": "1", "duration": f64::from(seconds) }] }
            })))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}/file/1")))
            .respond_with(ranged_response(silent_wav_bytes(seconds)))
            .mount(mock_server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(format!("/api/me/progress/{item_id}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(mock_server)
            .await;
    }

    /// Like `mock_playable_item`, but the item detail response also carries a chapter list — for
    /// tests exercising the chapters sheet, including tap-to-seek, which needs real, seekable
    /// audio (not just a parsed `get_item_playback_info` response).
    pub(crate) async fn mock_playable_item_with_chapters(
        mock_server: &MockServer,
        item_id: &str,
        seconds: u32,
        chapters: &[(&str, f64, f64)],
    ) {
        let chapters_json: Vec<_> = chapters
            .iter()
            .enumerate()
            .map(|(index, (title, start, end))| serde_json::json!({ "id": index, "start": start, "end": end, "title": title }))
            .collect();
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [{ "ino": "1", "duration": f64::from(seconds) }], "chapters": chapters_json }
            })))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}/file/1")))
            .respond_with(ranged_response(silent_wav_bytes(seconds)))
            .mount(mock_server)
            .await;
        Mock::given(method("PATCH"))
            .and(path(format!("/api/me/progress/{item_id}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(mock_server)
            .await;
    }

    pub(crate) async fn account_and_server(pool: &SqlitePool, server_url: &str) -> (abs_storage::models::Server, abs_storage::models::Account) {
        let server_id = abs_storage::repo::servers::add(pool, server_url).await.unwrap();
        let account_id = abs_storage::repo::accounts::add(pool, &server_id, "jane", "token123", None).await.unwrap();
        (
            abs_storage::repo::servers::get(pool, &server_id).await.unwrap(),
            abs_storage::repo::accounts::get(pool, &account_id).await.unwrap(),
        )
    }

    /// Progress rows have a foreign key on `(server_id, item_id)` referencing `items`, which
    /// itself references `libraries` — in the real app this is always satisfied (Home only makes
    /// an item clickable once it's synced), so tests exercising `PlayerController::start`'s
    /// progress-write path need to set up the same local rows first.
    pub(crate) async fn insert_synced_item(pool: &SqlitePool, server_id: &str, item_id: &str, title: &str) {
        abs_storage::repo::libraries::upsert(
            pool,
            abs_storage::repo::libraries::UpsertLibrary {
                id: "lib-1",
                server_id,
                name: "Audiobooks",
                media_type: "book",
                icon: None,
                display_order: 1,
            },
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

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point. Covers the whole real
    /// path: resolving a stream target over real HTTP (via wiremock), loading/playing it through a
    /// real GStreamer pipeline, publishing snapshots, pausing, and persisting progress.
    pub(crate) fn run_start_and_pause_persists_progress(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 3));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let seen: Rc<RefCell<Vec<PlayerSnapshot>>> = Rc::new(RefCell::new(Vec::new()));
        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), {
            let seen = seen.clone();
            move |snapshot: &PlayerSnapshot| seen.borrow_mut().push(snapshot.clone())
        });

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: Some("Some Author".to_string()) },
            1.0,
        );

        pump_until(|| seen.borrow().last().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        let first = seen.borrow().last().cloned().expect("starting playback should publish a snapshot");
        assert_eq!(first.title, "Test Book");
        assert_eq!(first.author.as_deref(), Some("Some Author"));
        assert!(first.is_playing, "starting playback should leave it playing");

        controller.pause();
        let after_pause = seen.borrow().last().cloned().unwrap();
        assert!(!after_pause.is_playing, "pause() should be reflected in the next snapshot");

        // `pause()` spawns the actual DB write; pump briefly so it lands before checking.
        pump_until(|| false, Duration::from_millis(300));
        let progress =
            runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap();
        assert!(progress.is_some(), "pausing should persist playback progress");
        assert!(!progress.unwrap().is_finished, "pausing mid-book must not mark it finished");

        // Progress isn't only local — it should also be pushed to the server, so it shows up in
        // the official apps and survives a fresh install.
        let synced_requests = runtime.block_on(mock_server.received_requests()).unwrap();
        let progress_sync = synced_requests
            .iter()
            .find(|r| r.method.as_str() == "PATCH" && r.url.path() == "/api/me/progress/item-1")
            .expect("pausing should also sync progress to the server");
        let body: serde_json::Value = progress_sync.body_json().unwrap();
        assert_eq!(body["isFinished"], false);
        // Once the server has it, the row no longer needs a push — otherwise every sync would
        // push it again.
        pump_until(
            || !runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap().needs_push,
            Duration::from_secs(5),
        );
        assert!(
            !runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap().needs_push,
            "a successful push must clear the row's needs-push mark"
        );
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Every server progress push reports
    /// how it ended, so the shell can tell the user when progress stops syncing. A 401 is told
    /// apart from other failures, since only logging in again fixes it.
    pub(crate) fn run_progress_sync_reports_each_outcome(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 30));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let outcomes: Rc<RefCell<Vec<ProgressSyncOutcome>>> = Rc::new(RefCell::new(Vec::new()));
        controller.set_on_progress_sync({
            let outcomes = outcomes.clone();
            move |outcome| outcomes.borrow_mut().push(outcome)
        });

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        outcomes.borrow_mut().clear();

        // Each one-shot, higher-priority response overrides the item's default 200 for exactly
        // one PATCH; `pause()` always forces a server push.
        let pause_with = |status: Option<u16>| {
            if let Some(status) = status {
                runtime.block_on(
                    Mock::given(method("PATCH"))
                        .and(path("/api/me/progress/item-1"))
                        .respond_with(ResponseTemplate::new(status))
                        .up_to_n_times(1)
                        .with_priority(1)
                        .mount(&mock_server),
                );
            }
            let seen = outcomes.borrow().len();
            controller.play();
            controller.pause();
            pump_until(|| outcomes.borrow().len() > seen, Duration::from_secs(5));
            outcomes.borrow().last().copied()
        };

        assert_eq!(pause_with(Some(500)), Some(ProgressSyncOutcome::Failed), "a server error is a retryable failure");
        assert_eq!(pause_with(Some(401)), Some(ProgressSyncOutcome::SessionExpired), "a 401 means the session is gone");
        assert_eq!(pause_with(None), Some(ProgressSyncOutcome::Synced), "a working server reports success");
        controller.stop();
    }

    /// Regression test for the "Play button silently does nothing" gap: when the audio engine
    /// itself is unavailable (no GStreamer plugins at all — `real_backend`'s `NullBackend`
    /// fallback, reproduced here as `FailingBackend` since `load()` always fails), `start()` used
    /// to `return` before ever constructing `now_playing` — leaving `current_download_context()`
    /// permanently `None` and, with it, `main_window`'s `start_playback` polling loop (which waits
    /// on exactly that) silently giving up after 5s with the Full Player screen never opening.
    /// `start()` now always builds `now_playing` (every piece of metadata it needs resolves before
    /// the load is even attempted) and records the failure on it instead.
    pub(crate) fn run_start_with_no_working_audio_engine_still_opens_with_an_error(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 3));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(FailingBackend::default()), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: Some("Some Author".to_string()) },
            1.0,
        );

        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        assert!(
            controller.current_download_context().is_some(),
            "now_playing must exist even when the backend can't load anything — this is what \
             unblocks the shell from opening the Full Player screen at all"
        );
        let snapshot = controller.snapshot().expect("now_playing should exist");
        assert_eq!(snapshot.title, "Test Book", "the real item metadata should still show, not a blank screen");
        assert!(!snapshot.is_playing);
        let err = snapshot.last_error.expect("a failed load must populate last_error");
        assert_eq!(err.kind, abs_player::PlaybackErrorKind::Unavailable);

        controller.stop();
    }

    /// Regression test for the "Play button silently does nothing" gap's more common trigger: the
    /// item was never played or downloaded on this device (no cached track metadata to fall back
    /// on) and the server rejects the resolve with a 401 — an expired session, in practice.
    /// `start()` used to `return` before `now_playing` existed at all in this case too; it now
    /// records the failure with `PlaybackErrorKind::NotAuthorized` (via `resolve_stream_target`
    /// mapping a 401 to `CoreError::Auth`), same as any other failed load.
    pub(crate) fn run_start_with_an_expired_session_and_nothing_cached_shows_a_login_error(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(async {
            Mock::given(method("GET")).and(path("/api/items/item-1")).respond_with(ResponseTemplate::new(401)).mount(&mock_server).await;
            Mock::given(method("GET")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(404)).mount(&mock_server).await;
        });

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );

        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        assert!(
            controller.current_download_context().is_some(),
            "now_playing must exist even when the resolve fails entirely — otherwise the shell's \
             readiness poll expires and the Full Player screen never opens at all"
        );
        let snapshot = controller.snapshot().expect("now_playing should exist");
        assert_eq!(snapshot.title, "Test Book");
        assert!(!snapshot.is_playing);
        let err = snapshot.last_error.expect("a failed resolve must populate last_error");
        assert_eq!(err.kind, abs_player::PlaybackErrorKind::NotAuthorized, "a 401 should read as a login failure, not a generic network one");

        controller.stop();
    }

    /// Regression test for the "no flush on teardown" gap: switching to a new item while another
    /// is still actively playing used to lose whatever progress had accrued since the last
    /// periodic local write (the only other write paths are the tick and an explicit `pause()`,
    /// neither of which fires on a session switch). `start()` now flushes the outgoing item first,
    /// and always forces a server sync regardless of `SERVER_PROGRESS_SYNC_INTERVAL`.
    pub(crate) fn run_starting_a_new_item_flushes_the_previous_items_progress(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));
        runtime.block_on(mock_playable_item(&mock_server, "item-2", 5));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "First Book"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Second Book"));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "First Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        // Let some real playback time accrue, but nowhere near `LOCAL_PROGRESS_WRITE_INTERVAL`
        // (5s) — if the periodic tick were what persisted this, the test would be proving nothing.
        pump_until(|| false, Duration::from_millis(1200));

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-2".to_string(), title: "Second Book".to_string(), author: None },
            1.0,
        );
        // The flush is a synchronous call inside `start()`, but the DB write/server sync it
        // spawns still needs a pump to land — waited for, not timed: a fixed 300 ms was sometimes
        // too short for the push.
        let pushed = || {
            runtime
                .block_on(mock_server.received_requests())
                .unwrap()
                .iter()
                .any(|r| r.method.as_str() == "PATCH" && r.url.path() == "/api/me/progress/item-1")
        };
        pump_until(pushed, Duration::from_secs(5));

        let progress = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap();
        let progress = progress.expect("switching items should flush the outgoing item's progress locally");
        assert!(progress.current_time_seconds > 0.0, "the flushed position should reflect real elapsed playback, got {}", progress.current_time_seconds);

        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(
            requests.iter().any(|r| r.method.as_str() == "PATCH" && r.url.path() == "/api/me/progress/item-1"),
            "switching items should also sync the outgoing item's progress to the server"
        );
        controller.stop();
    }

    /// Regression test for the "no reconnect-triggered sync" gap:
    /// `PlayerController::sync_pending_progress` is the one-line closure
    /// `screens::main_window::build` wires to `NetworkManagerConnectivityWatcher`'s "connectivity
    /// restored" callback (see `run_connectivity_restored_wiring_syncs_pending_progress` in
    /// `screens::main_window::tests` for the wiring itself) — this covers the method's own logic:
    /// when the server has nothing newer, the local (paused) position gets pushed.
    pub(crate) fn run_sync_pending_progress_pushes_the_current_position_while_paused(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));
        // No existing server-side progress — the reconcile GET should just find nothing to pull.
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        // Offline at the moment of pausing: that push fails, so the position is still pending.
        runtime.block_on(
            Mock::given(method("PATCH"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(ResponseTemplate::new(503))
                .with_priority(1)
                .up_to_n_times(1)
                .mount(&mock_server),
        );
        controller.pause();
        pump_until(|| false, Duration::from_millis(300));

        let requests_before_reconnect = runtime.block_on(mock_server.received_requests()).unwrap().len();

        controller.sync_pending_progress();
        pump_until(|| false, Duration::from_millis(500));

        let requests_after = runtime.block_on(mock_server.received_requests()).unwrap();
        let new_patches = requests_after[requests_before_reconnect..]
            .iter()
            .filter(|r| r.method.as_str() == "PATCH" && r.url.path() == "/api/me/progress/item-1")
            .count();
        assert_eq!(new_patches, 1, "reconnecting while paused should push exactly one pending progress update");
        controller.stop();
    }

    /// The direct regression test for the collision risk raised while reviewing this fix: if
    /// another device advanced this item's progress on the server while this device sat
    /// paused/offline, `sync_pending_progress` must NOT blindly push this device's stale local
    /// position over it — it must reconcile first and skip the push once the newer server value
    /// has been applied locally.
    pub(crate) fn run_sync_pending_progress_does_not_clobber_a_newer_server_value(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 500));
        // The server's own record is far ahead of (and newer than) whatever this device paused
        // at — as if another device kept listening while this one was offline.
        let newer_update = chrono::Utc::now().timestamp_millis() + 60_000;
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraryItemId": "item-1",
                    "currentTime": 400.0,
                    "duration": 500.0,
                    "isFinished": false,
                    "lastUpdate": newer_update,
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        controller.pause();
        pump_until(|| false, Duration::from_millis(300));

        let requests_before_reconnect = runtime.block_on(mock_server.received_requests()).unwrap().len();

        controller.sync_pending_progress();
        pump_until(|| false, Duration::from_millis(500));

        let progress = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert_eq!(progress.current_time_seconds, 400.0, "reconciling should have pulled the newer server-side position locally");

        let requests_after = runtime.block_on(mock_server.received_requests()).unwrap();
        let new_patches = requests_after[requests_before_reconnect..]
            .iter()
            .filter(|r| r.method.as_str() == "PATCH" && r.url.path() == "/api/me/progress/item-1")
            .count();
        assert_eq!(new_patches, 0, "the stale local position must never be pushed back over a newer server value");
        controller.stop();
    }

    /// `sync_pending_progress` is a no-op if nothing is loaded — must not panic.
    pub(crate) fn run_sync_pending_progress_is_a_no_op_when_nothing_is_loaded(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let controller = PlayerController::new(pool, crate::test_support::test_paths(), test_backend(), |_| {});
        controller.sync_pending_progress();
        pump_until(|| false, Duration::from_millis(100));
        controller.stop();
    }

    /// Regression test for a real bug caught in manual live testing (not by any prior test — this
    /// path had no coverage at all): `start()` requested a seek immediately after requesting
    /// `pause()`, but a seek needs the pipeline to have *reached* `PAUSED`, not just been asked to
    /// — for a network-streamed source that transition isn't instant, so the seek silently
    /// no-opped and "resume mid-book" quietly restarted from 0 instead. Fixed by polling for
    /// `duration()` to become available (this crate's own signal that PAUSED was actually reached
    /// — see `abs-player`'s own tests) before attempting the resume seek.
    pub(crate) fn run_start_resumes_from_existing_progress(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 10));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 6.0, false)).unwrap();

        let seen: Rc<RefCell<Vec<PlayerSnapshot>>> = Rc::new(RefCell::new(Vec::new()));
        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), {
            let seen = seen.clone();
            move |snapshot: &PlayerSnapshot| seen.borrow_mut().push(snapshot.clone())
        });

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );

        pump_until(|| seen.borrow().last().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        // A `FLUSH` seek's position update lands on the pipeline's own streaming thread, not
        // synchronously inside `seek()` — give it a brief moment to actually land. 500ms of real
        // (sync'd) playback is nowhere near enough to "catch up" from 0 to past 5s on its own, so
        // this can't pass by coincidence if the resume seek silently no-opped.
        pump_until(|| false, Duration::from_millis(500));
        let position = seen.borrow().last().unwrap().position_seconds;
        assert!(position >= 5.0, "starting an item with existing progress should resume near it, not from 0 (got {position}s)");
        controller.stop();
    }

    /// Like `mock_playable_item`, but the item is split across two audio files — the shape the
    /// multi-track sequencing scenarios need. Both files are served as short, real, ranged WAVs
    /// so end-of-stream handover and cross-file seeking exercise actual GStreamer pipelines.
    pub(crate) async fn mock_two_track_item(mock_server: &MockServer, seconds_per_track: u32) {
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [
                    { "ino": "1", "duration": f64::from(seconds_per_track) },
                    { "ino": "2", "duration": f64::from(seconds_per_track) },
                ] }
            })))
            .mount(mock_server)
            .await;
        for ino in ["1", "2"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/items/item-1/file/{ino}")))
                .respond_with(ranged_response(silent_wav_bytes(seconds_per_track)))
                .mount(mock_server)
                .await;
        }
        Mock::given(method("PATCH"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(200))
            .mount(mock_server)
            .await;
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The server's timeline is canonical:
    /// when a file's real length differs from the one the server reported (seeded here: the server
    /// says 10s, the real WAV is 2s), the offsets and the book total stay as the server has them
    /// — saved positions are read back against that timeline by every client — and the reported
    /// position never runs past the end of the file in the server's terms.
    pub(crate) fn run_track_duration_mismatch_leaves_the_server_timeline_alone(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/api/items/item-1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    // Track 1 is declared as 10s but the WAV actually served is only 2s — the
                    // kind of inaccurate server metadata this correction exists to tolerate.
                    "media": { "audioFiles": [
                        { "ino": "1", "duration": 10.0 },
                        { "ino": "2", "duration": 2.0 },
                    ] }
                })))
                .mount(&mock_server)
                .await;
            for ino in ["1", "2"] {
                Mock::given(method("GET"))
                    .and(path(format!("/api/items/item-1/file/{ino}")))
                    .respond_with(ranged_response(silent_wav_bytes(2)))
                    .mount(&mock_server)
                    .await;
            }
            Mock::given(method("PATCH"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&mock_server)
                .await;
        });

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Multi-track Book".to_string(), author: None },
            1.0,
        );

        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        assert_eq!(controller.snapshot().unwrap().duration_seconds, 12.0);

        // The real first file (2s) ends and hands over to the second. Nothing is shifted.
        pump_until(
            || {
                let requests = runtime.block_on(mock_server.received_requests()).unwrap();
                requests.iter().any(|r| r.method.as_str() == "GET" && r.url.path() == "/api/items/item-1/file/2")
            },
            Duration::from_secs(15),
        );
        pump_until(|| false, Duration::from_millis(200));
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.duration_seconds, 12.0, "the book total stays what the server reported");
        assert!(snapshot.position_seconds >= 10.0, "the second file starts at the server's offset, got {}", snapshot.position_seconds);

        controller.stop();
    }

    /// Seeds the `tracks` rows (the cached per-file metadata `sync_item_tracks` normally writes
    /// after resolving) for an item's full track list in one go — one call, not one per track,
    /// since `upsert_all` *replaces* the item's whole list.
    async fn seed_track_metadata(pool: &SqlitePool, server_id: &str, item_id: &str, tracks: &[(&str, f64, f64)]) {
        let rows: Vec<abs_storage::repo::tracks::NewTrack<'_>> = tracks
            .iter()
            .map(|&(ino, duration_seconds, offset_seconds)| abs_storage::repo::tracks::NewTrack { ino, duration_seconds, offset_seconds, size_bytes: None })
            .collect();
        abs_storage::repo::tracks::upsert_all(pool, server_id, item_id, &rows).await.unwrap();
    }

    /// Seeds a `Complete` `download_tracks` row for `ino` pointing at a real file written with
    /// `bytes` — the exact "trustworthy" shape `local_track_path`/`verified_complete_path` require
    /// (status `Complete`, on-disk size matching `expected_size_bytes`). The item's `tracks` rows
    /// must already exist (via `seed_track_metadata` — the download table has a foreign key on
    /// them, normally written by `sync_item_tracks` before any download starts).
    async fn seed_downloaded_track(pool: &SqlitePool, paths: &AppPaths, server_id: &str, item_id: &str, ino: &str, bytes: &[u8]) {
        let path = paths.track_file_path(server_id, item_id, ino, "wav");
        tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&path, bytes).await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(pool, server_id, item_id, ino, path.to_str().unwrap()).await.unwrap();
        abs_storage::repo::download_tracks::mark_complete(pool, server_id, item_id, ino, bytes.len() as i64).await.unwrap();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A track that's already been
    /// downloaded (a `Complete` row with a real, correctly-sized file on disk) must be played from
    /// that local file instead of streaming it again — asserted by checking the file endpoint was
    /// never actually requested, not just that playback worked.
    pub(crate) fn run_downloaded_track_is_preferred_over_streaming(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 3));

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 3.0, 0.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(3)));

        let controller = PlayerController::new(pool.clone(), paths, test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool, &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.duration_seconds > 0.0), Duration::from_secs(10));
        // Give playback a real moment to run — long enough that a streamed track would definitely
        // have issued its HTTP request by now.
        pump_until(|| false, Duration::from_millis(500));

        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(
            !requests.iter().any(|r| r.url.path() == "/api/items/item-1/file/1"),
            "a downloaded track must be played from disk, not re-streamed"
        );
        assert!(controller.snapshot().unwrap().source_is_local, "the snapshot says the track plays from the device");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A `Complete` row whose file has gone
    /// missing (deleted externally after the row was written) must not be trusted — playback falls
    /// back to streaming rather than failing to load anything.
    pub(crate) fn run_untrustworthy_complete_row_falls_back_to_streaming(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 3));

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 3.0, 0.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(3)));
        // Delete the file after the row was written — the row still says `Complete`, but it's a
        // lie now.
        let stale_path = paths.track_file_path(&server.id, "item-1", "1", "wav");
        runtime.block_on(tokio::fs::remove_file(&stale_path)).unwrap();

        let controller = PlayerController::new(pool.clone(), paths, test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool, &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(
            || runtime.block_on(mock_server.received_requests()).unwrap().iter().any(|r| r.url.path() == "/api/items/item-1/file/1"),
            Duration::from_secs(10),
        );
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(
            requests.iter().any(|r| r.url.path() == "/api/items/item-1/file/1"),
            "an untrustworthy Complete row (missing file) must fall back to streaming"
        );
        assert!(!controller.snapshot().unwrap().source_is_local, "the snapshot says the track is streamed");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Mixed state across a two-track item:
    /// the first track is downloaded, the second isn't. Starting plays the first from disk (no
    /// request), and crossing into the second (via `spawn_load_track`) streams it (a request).
    pub(crate) fn run_multi_track_mixed_downloaded_and_streamed(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, 2));

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 2.0, 0.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(2)));

        let controller = PlayerController::new(pool.clone(), paths, test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool, &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Multi-track Book".to_string(), author: None },
            1.0,
        );

        // The first track plays from disk — give it a real moment, then confirm no request for
        // its file landed, before letting it run on into the second (streamed) track.
        pump_until(|| controller.snapshot().is_some_and(|s| s.duration_seconds > 0.0), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(500));
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(!requests.iter().any(|r| r.url.path() == "/api/items/item-1/file/1"), "the downloaded first track must not be streamed");

        pump_until(
            || runtime.block_on(mock_server.received_requests()).unwrap().iter().any(|r| r.url.path() == "/api/items/item-1/file/2"),
            Duration::from_secs(15),
        );
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(requests.iter().any(|r| r.url.path() == "/api/items/item-1/file/2"), "the not-yet-downloaded second track must be streamed");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Offline start of a fully-downloaded
    /// item: the server can't even serve the item's playback info (nothing is mounted, so
    /// wiremock 404s it), but every track is on disk — playback must start entirely from the
    /// local files, with the book duration coming from the locally cached track metadata, and
    /// never touch the server's file endpoint.
    pub(crate) fn run_fully_downloaded_item_plays_offline(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 3.0, 0.0), ("2", 2.0, 3.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(3)));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "2", &silent_wav_bytes(2)));

        let controller = PlayerController::new(pool.clone(), paths, test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool, &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Offline Book".to_string(), author: None },
            1.0,
        );

        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        let snapshot = controller.snapshot().unwrap();
        assert!(snapshot.is_playing, "a fully-downloaded item must play offline");
        assert_eq!(snapshot.duration_seconds, 5.0, "the book total must come from the locally cached track metadata, not the failed resolve");

        // Give playback a real moment — long enough that a streamed track would definitely have
        // issued its HTTP request by now — then confirm it never did.
        pump_until(|| false, Duration::from_millis(500));
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(
            !requests.iter().any(|r| r.url.path().starts_with("/api/items/item-1/file/")),
            "a fully-downloaded item must play entirely offline"
        );
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Offline start of a partially-
    /// downloaded item: the downloaded first track plays from disk; when it ends and playback
    /// crosses into the not-downloaded second track, the streaming load fails (the server is
    /// unreachable) and playback stops at the gap through the existing error path — instead of
    /// the item failing to start at all.
    pub(crate) fn run_partially_downloaded_item_plays_until_a_gap_offline(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 2.0, 0.0), ("2", 2.0, 2.0)]));
        // Only the first track is downloaded; the second has metadata but no file, no download row.
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(2)));

        let controller = PlayerController::new(pool.clone(), paths, test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool, &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Half Offline Book".to_string(), author: None },
            1.0,
        );

        // The downloaded track plays from disk — never streamed.
        pump_until(|| controller.snapshot().is_some_and(|s| s.duration_seconds > 0.0), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(500));
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(!requests.iter().any(|r| r.url.path() == "/api/items/item-1/file/1"), "the downloaded track must not be streamed");

        // Crossing into the gap reaches for the server, fails, and stops playback.
        pump_until(
            || runtime.block_on(mock_server.received_requests()).unwrap().iter().any(|r| r.url.path() == "/api/items/item-1/file/2"),
            Duration::from_secs(15),
        );
        pump_until(|| !controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point. Multi-file items used to
    /// stop dead (and mark the whole book finished) at the first file's end, with a caveat note
    /// saying so; the scenarios that follow pin the sequential playback that replaced that.
    pub(crate) fn run_multi_track_advances_to_the_next_track(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, 2));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let seen: Rc<RefCell<Vec<PlayerSnapshot>>> = Rc::new(RefCell::new(Vec::new()));
        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), {
            let seen = seen.clone();
            move |snapshot: &PlayerSnapshot| seen.borrow_mut().push(snapshot.clone())
        });

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Multi-track Book".to_string(), author: None },
            1.0,
        );

        // Track 1 is 2s: the book-level position must keep going past it into the second track's
        // range, not stop at the boundary.
        pump_until(|| seen.borrow().last().is_some_and(|s| s.position_seconds >= 2.5), Duration::from_secs(15));
        assert!(seen.borrow().last().unwrap().is_playing, "advancing to the next track should not stop playback");

        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(
            requests.iter().any(|r| r.method.as_str() == "GET" && r.url.path() == "/api/items/item-1/file/2"),
            "the second file should actually have been fetched"
        );

        controller.pause();
        pump_until(|| false, Duration::from_millis(300));
        let progress =
            runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap();
        assert!(!progress.unwrap().is_finished, "reaching the first track's end must not mark the book finished");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Only the *last* track's end-of-stream
    /// is the book's end: a very short two-track clip so both handover and the final stop arrive
    /// quickly, then progress must read finished at the book-level end.
    pub(crate) fn run_multi_track_final_track_marks_finished(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, 1));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let seen: Rc<RefCell<Vec<PlayerSnapshot>>> = Rc::new(RefCell::new(Vec::new()));
        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), {
            let seen = seen.clone();
            move |snapshot: &PlayerSnapshot| seen.borrow_mut().push(snapshot.clone())
        });

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Multi-track Book".to_string(), author: None },
            1.0,
        );

        // `is_playing` stays true through the first track's handover, so the first false reading
        // is the final end-of-stream.
        pump_until(|| seen.borrow().last().is_some_and(|s| !s.is_loading && !s.is_playing), Duration::from_secs(15));

        pump_until(|| false, Duration::from_millis(300));
        let progress =
            runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert!(progress.is_finished, "the final track's end should mark the book finished");
        assert!(
            progress.current_time_seconds >= 1.9,
            "should be recorded at the book-level end, got {}",
            progress.current_time_seconds
        );
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Seeking to a book-level position
    /// inside the second file must load that file and land the pipeline at the mapped in-track
    /// offset — asserted while paused, so a correct seek *stays* at its target rather than
    /// drifting (which is also what keeps this from passing by natural playback reaching the
    /// target on its own).
    pub(crate) fn run_seek_across_track_boundary_lands_in_the_next_file(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, 3));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Multi-track Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds > 0.5), Duration::from_secs(10));
        controller.pause();
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading && !s.is_playing), Duration::from_secs(5));

        controller.seek_to_seconds(4.0); // 1s into the second 3s file
        // The position reads as the target straight away (it is pinned there until the second
        // file has loaded), so wait on the fetch itself, then on the seek landing.
        let fetched_second_file = || {
            runtime
                .block_on(mock_server.received_requests())
                .unwrap()
                .iter()
                .any(|r| r.method.as_str() == "GET" && r.url.path() == "/api/items/item-1/file/2")
        };
        pump_until(fetched_second_file, Duration::from_secs(10));
        assert!(fetched_second_file(), "seeking into the second track should fetch the second file");
        pump_until(|| false, Duration::from_secs(1));
        let position = controller.snapshot().unwrap().position_seconds;
        assert!((3.9..=4.5).contains(&position), "a paused cross-track seek should land at its target and stay there (got {position})");
        controller.stop();
    }


    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Regression test for the "went
    /// backwards" half of this plan's Librem 5 field report: `souphttpsrc` reports no position
    /// at all for a real stretch of time after a `FLUSH` seek on a streamed file (until the new
    /// range response arrives), and the old code read that as "start of the current track" —
    /// this seeds exactly that stall (via `ranged_response_delayed_when_seeking`) and asserts
    /// every snapshot published during it still reads near the seek's own target, never near 0.
    pub(crate) fn run_stalled_seek_keeps_the_last_known_position(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        let seconds = 20u32;
        runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/api/items/item-1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "media": { "audioFiles": [{ "ino": "1", "duration": f64::from(seconds) }] }
                })))
                .mount(&mock_server)
                .await;
            Mock::given(method("GET"))
                .and(path("/api/items/item-1/file/1"))
                .respond_with(ranged_response_delayed_when_seeking(silent_wav_bytes(seconds), Duration::from_secs(2)))
                .mount(&mock_server)
                .await;
            Mock::given(method("PATCH"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&mock_server)
                .await;
        });

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        controller.skip(10.0);
        // The seek's own range request is now stalled for ~2s (`ranged_response_delayed_when_seeking`);
        // sample every snapshot for a good chunk of that window and require every single one to
        // still read near the skip's target — a single bad sample reading near 0 is exactly the
        // "went backwards" bug this test guards against.
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        let mut min_seen = f64::MAX;
        while std::time::Instant::now() < deadline {
            if let Some(snapshot) = controller.snapshot() {
                min_seen = min_seen.min(snapshot.position_seconds);
            }
            pump_until(|| false, Duration::from_millis(50));
        }
        assert!(min_seen >= 9.0, "position must never read near 0 while a seek's range request is stalled (min seen: {min_seen}s)");
        controller.stop();
    }

    /// A minimal, fully scripted `AudioBackend` for testing the error/retry *state machine*
    /// itself (`tick`'s `Error` handling, `needs_reload`, `play()`'s reload path) without
    /// depending on real GStreamer/network timing — a real pipeline over a small test WAV tends
    /// to buffer the whole file from its first request, so a later seek never issues a second
    /// HTTP request at all and a mocked failure response is never actually reached. Shared
    /// `ScriptedBackendState` lets the test both drive it (queue a `PlayerEvent`, move the
    /// simulated position) and inspect what the controller asked of it (`load`/`seek` calls).
    #[derive(Default)]
    struct ScriptedBackendState {
        position: Option<Duration>,
        load_calls: Vec<String>,
        seek_calls: Vec<Duration>,
        reset_calls: u32,
        pending_event: Option<abs_player::PlayerEvent>,
        /// How many upcoming `seek` calls to record but not act on — a seek asked for before a
        /// real pipeline can take it silently does nothing, which is what this simulates.
        seeks_to_ignore: u32,
        /// When set, `is_paused()` reports `false` forever — simulating a pipeline stuck in an
        /// async, never-completing `Paused` transition (`pause()` itself still returns `Ok`,
        /// same as a real pipeline's `Async` result). Defaults to `false` (not stuck) so every
        /// pre-existing test using `ScriptedBackend` is unaffected: its `pause()` "succeeding"
        /// is immediately confirmed, exactly as if a real pipeline settled right away.
        stuck_paused: bool,
        /// When set, a `load()` leaves `position()` at `None` — the pipeline hasn't prerolled —
        /// until the test sets a position, so a load's readiness wait really waits.
        hold_preroll: bool,
        play_calls: u32,
        speed_calls: Vec<(f64, Duration)>,
    }

    struct ScriptedBackend(Rc<RefCell<ScriptedBackendState>>);

    impl abs_player::AudioBackend for ScriptedBackend {
        fn load(&mut self, uri: &str) -> abs_player::Result<()> {
            let mut state = self.0.borrow_mut();
            state.load_calls.push(uri.to_string());
            state.position = if state.hold_preroll { None } else { Some(Duration::ZERO) };
            Ok(())
        }
        fn apply_connection(&mut self, _properties: &abs_player::ConnectionProperties) {}
        fn play(&mut self) -> abs_player::Result<()> {
            self.0.borrow_mut().play_calls += 1;
            Ok(())
        }
        fn pause(&mut self) -> abs_player::Result<()> {
            Ok(())
        }
        fn is_paused(&self) -> bool {
            !self.0.borrow().stuck_paused
        }
        fn seek(&mut self, position: Duration) -> abs_player::Result<()> {
            let mut state = self.0.borrow_mut();
            state.seek_calls.push(position);
            if state.seeks_to_ignore > 0 {
                state.seeks_to_ignore -= 1;
            } else {
                state.position = Some(position);
            }
            Ok(())
        }
        fn set_speed(&mut self, speed: f64, position: Duration) -> abs_player::Result<()> {
            let mut state = self.0.borrow_mut();
            state.speed_calls.push((speed, position));
            state.position = Some(position);
            Ok(())
        }
        fn position(&self) -> Option<Duration> {
            self.0.borrow().position
        }
        fn duration(&self) -> Option<Duration> {
            Some(Duration::from_secs(20))
        }
        fn poll_event(&self) -> Option<abs_player::PlayerEvent> {
            self.0.borrow_mut().pending_event.take()
        }
        fn set_burst_buffering(&mut self, _enabled: bool) {}
        fn reset(&mut self) {
            let mut state = self.0.borrow_mut();
            state.reset_calls += 1;
            state.position = None;
        }
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Regression test for the "second tap
    /// killed playback" half of this plan's Librem 5 field report: a bus `Error` must release
    /// the backend (`AudioBackend::reset`) rather than leave a dead pipeline that every later
    /// Play/Retry just re-asks to resume — and once the network is back, Retry (`play()`) must
    /// reload from the position the error left off at, not from 0.
    pub(crate) fn run_error_then_retry_reloads_from_the_last_good_position(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        // Starting releases whatever the backend held before; count only what follows.
        let resets_after_start = state.borrow().reset_calls;

        // The scripted backend has no real clock — move its position directly to simulate
        // playback having advanced, then let a tick observe it via `observe_position`.
        state.borrow_mut().position = Some(Duration::from_secs(3));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 2.5), Duration::from_secs(10));

        // Inject a bus error, as if the network died mid-stream.
        // The automatic reload is covered by its own test; this one is about what follows it.
        controller.use_up_auto_recovery();
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::Error(abs_player::PlaybackError {
            kind: abs_player::PlaybackErrorKind::Network,
            message: "simulated network failure".to_string(),
            debug: None,
        }));
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(10));

        let snapshot = controller.snapshot().unwrap();
        assert!(!snapshot.is_playing, "a stream error must leave playback paused");
        assert!(
            (snapshot.position_seconds - 3.0).abs() < 0.5,
            "the position at the moment of the error must be the last known-good one, not 0 (got {})",
            snapshot.position_seconds
        );
        assert_eq!(
            state.borrow().reset_calls,
            resets_after_start + 1,
            "a bus error must release the backend (AudioBackend::reset) rather than leave it holding a dead pipeline"
        );

        // Retry: must reload (a fresh `load()` call to the backend), not just re-ask a dead
        // pipeline to resume.
        let load_calls_before_retry = state.borrow().load_calls.len();
        controller.play();
        // `play()` sets `is_playing`/clears `last_error` synchronously, before the reload it
        // kicks off has actually run — the reload landing is only observable through the
        // backend's own `load_calls`, so that's what this waits on rather than the snapshot.
        pump_until(|| state.borrow().load_calls.len() > load_calls_before_retry, Duration::from_secs(10));

        assert_eq!(
            state.borrow().load_calls.len(),
            load_calls_before_retry + 1,
            "Retry must reload the track from scratch, not just ask a dead pipeline to resume"
        );
        assert!(
            controller.snapshot().is_some_and(|s| s.is_playing && s.last_error.is_none()),
            "once the reload lands, playback should be running with no error showing"
        );
        assert_eq!(
            state.borrow().seek_calls.last().copied(),
            Some(Duration::from_secs(3)),
            "the reload must seek back to the position the error left off at, not start from 0"
        );
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Regression test for the Librem 5
    /// field report `pause()`'s doc comment describes: `backend.pause()` returning `Ok` only
    /// means GStreamer *accepted* the request, not that the pipeline actually reached `Paused` —
    /// a network-streamed source mid-buffer-read can sit `Async` indefinitely. Simulated here via
    /// `ScriptedBackend`'s `stuck_paused` flag (`is_paused()` never returns `true`, exactly like
    /// a pipeline stuck in `Async`), since a real `GstBackend` has no seam to force that
    /// deterministically. `PlayerController::pause()`'s confirmation poll must eventually notice
    /// and recover: mark the item for reload, surface an error, and release the backend — the
    /// same recovery an outright `backend.pause()` failure already gets.
    pub(crate) fn run_a_pause_that_never_lands_is_recovered(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        // Starting releases whatever the backend held before; count only what follows.
        let resets_after_start = state.borrow().reset_calls;

        state.borrow_mut().stuck_paused = true;
        controller.pause();
        // `pause()` itself reports success immediately (`backend.pause()` returned `Ok`, exactly
        // like a real pipeline's `Async` result) — the bug this fixes is that nothing used to
        // check any further than that.
        let snapshot = controller.snapshot().unwrap();
        assert!(!snapshot.is_playing, "pause() should still report not-playing right away");
        assert!(snapshot.last_error.is_none(), "no error yet — the confirmation window hasn't elapsed");
        assert_eq!(state.borrow().reset_calls, resets_after_start, "not recovered yet — still within the confirmation window");

        // `PAUSE_CONFIRM_INTERVAL * PAUSE_CONFIRM_ATTEMPTS` is the whole window; give it a wide
        // margin rather than pinning the exact constants here.
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(10));

        let snapshot = controller.snapshot().unwrap();
        assert!(!snapshot.is_playing, "must stay paused (not silently reported as playing)");
        assert!(snapshot.last_error.is_some(), "a pause that never lands must eventually surface as an error");
        assert_eq!(state.borrow().reset_calls, resets_after_start + 1, "a pause stuck forever must release the backend, same as an outright pause failure");

        // The item must be recoverable exactly like any other `needs_reload` case: the next
        // `play()` reloads rather than trying to resume a pipeline that was already released.
        let load_calls_before_retry = state.borrow().load_calls.len();
        controller.play();
        pump_until(|| state.borrow().load_calls.len() > load_calls_before_retry, Duration::from_secs(10));
        assert!(
            controller.snapshot().is_some_and(|s| s.is_playing && s.last_error.is_none()),
            "once reloaded, playback should be running again with no error showing"
        );
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Regression test for the Librem 5
    /// field report `PlayerController::external_play_pause` describes: unplugging a TRRS headset
    /// makes the desktop send an MPRIS `PlayPause` ~2ms after the unplug pause, which used to
    /// toggle playback straight back on. It must be ignored — but only right after an *unplug*
    /// pause: a later `PlayPause`, or one right after a manual pause, must still toggle.
    pub(crate) fn run_mpris_play_pause_right_after_an_unplug_is_ignored(runtime: &tokio::runtime::Runtime) {
        use abs_player::mpris::MprisCommands;

        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.set_headphone_behavior(true, false);
        let mpris = MprisBridge::new(controller.clone());
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        // The unplug, immediately followed by the headset-button `PlayPause` the plug generates.
        controller.handle_route_event(abs_player::route_watch::RouteEvent::Unplugged);
        mpris.play_pause();
        assert!(!controller.snapshot().unwrap().is_playing, "a PlayPause right after an unplug pause must not resume playback");

        // Well past the window, a PlayPause is a real press again.
        pump_until(|| false, SPURIOUS_UNPLUG_TOGGLE_WINDOW + Duration::from_millis(200));
        mpris.play_pause();
        assert!(controller.snapshot().unwrap().is_playing, "a PlayPause well after the unplug must toggle as usual");

        // A manual pause is not an unplug pause: a PlayPause right after it must still toggle.
        controller.pause();
        mpris.play_pause();
        assert!(controller.snapshot().unwrap().is_playing, "the guard must only apply to an unplug pause, never to a manual one");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A resume seek asked for before the
    /// pipeline could take it silently does nothing. The position must not be read (or saved) as
    /// the start of the track meanwhile, and the seek must be asked for again until it lands.
    pub(crate) fn run_a_resume_seek_that_does_not_land_is_reissued(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 10.0, false)).unwrap();

        let state = Rc::new(RefCell::new(ScriptedBackendState { seeks_to_ignore: 1, ..Default::default() }));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        assert_eq!(state.borrow().position, Some(Duration::ZERO), "the scripted first seek must have been ignored");

        let min_seen = std::cell::Cell::new(f64::MAX);
        let landed = || state.borrow().seek_calls.len() >= 2 && state.borrow().position == Some(Duration::from_secs(10));
        pump_until(
            || {
                if let Some(snapshot) = controller.snapshot() {
                    min_seen.set(min_seen.get().min(snapshot.position_seconds));
                }
                landed()
            },
            Duration::from_secs(10),
        );
        let min_seen = min_seen.get();
        assert!(landed(), "the missed resume seek must be asked for again: {:?}", state.borrow().seek_calls);
        assert!(min_seen >= 9.5, "the position must never read as the start of the track while the seek is outstanding (min seen: {min_seen}s)");

        // Let a periodic write happen, then check nothing below the resume point was saved.
        state.borrow_mut().position = Some(Duration::from_secs(11));
        pump_until(|| false, LOCAL_PROGRESS_WRITE_INTERVAL + Duration::from_secs(1));
        let saved = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert!(saved.current_time_seconds >= 10.0, "saved progress must not fall back below the resume point: {}", saved.current_time_seconds);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A stream error followed by starting a
    /// book that can't resolve must keep the first book's position, must stop the first book's
    /// audio, and Retry must re-run the start instead of resuming the first book's pipeline under
    /// the second book's title (which would then save the first book's position as the second's).
    pub(crate) fn run_a_failed_start_keeps_both_books_positions(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        runtime.block_on(Mock::given(method("GET")).and(path("/api/items/item-2")).respond_with(ResponseTemplate::new(500)).mount(&mock_server));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Book Two"));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-2", 500.0, false)).unwrap();

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        controller.start(session.clone(), PlayRequest { item_id: "item-1".to_string(), title: "Book One".to_string(), author: None }, 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(3));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 2.5), Duration::from_secs(10));

        // The stream dies: the backend is released and reports no position from here on.
        // The automatic reload is covered by its own test; this one is about what follows it.
        controller.use_up_auto_recovery();
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::Error(abs_player::PlaybackError {
            kind: abs_player::PlaybackErrorKind::Network,
            message: "simulated network failure".to_string(),
            debug: None,
        }));
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(10));
        let resets_before = state.borrow().reset_calls;

        controller.start(session.clone(), PlayRequest { item_id: "item-2".to_string(), title: "Book Two".to_string(), author: None }, 1.0);
        let failed = || controller.snapshot().is_some_and(|s| s.title == "Book Two" && s.last_error.is_some());
        pump_until(failed, Duration::from_secs(10));
        assert!(failed(), "starting an item that can't resolve should end in an error state");
        assert!(state.borrow().reset_calls > resets_before, "a failed start must release whatever the backend still held");
        pump_until(|| false, Duration::from_millis(300));

        let progress = |item_id: &str| {
            runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, item_id)).unwrap().unwrap().current_time_seconds
        };
        assert!((progress("item-1") - 3.0).abs() < 0.5, "the first book keeps the position it errored at, got {}", progress("item-1"));
        assert_eq!(progress("item-2"), 500.0, "the failed book's saved position must be untouched");

        // Retry re-runs the start (a second resolve request), and the pipeline isn't resumed.
        let resolves = || {
            runtime.block_on(mock_server.received_requests()).unwrap().iter().filter(|r| r.url.path() == "/api/items/item-2").count()
        };
        let resolves_before = resolves();
        let loads_before = state.borrow().load_calls.len();
        controller.play();
        pump_until(|| resolves() > resolves_before, Duration::from_secs(10));
        assert!(resolves() > resolves_before, "Retry after a failed start must resolve the item again");
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(300));
        assert_eq!(state.borrow().load_calls.len(), loads_before, "nothing may be loaded for an item that still can't resolve");
        assert!(!controller.snapshot().unwrap().is_playing);
        assert_eq!(progress("item-2"), 500.0, "Retry must not write anything for the failed book");
        controller.stop();
    }

    fn progress_patches(runtime: &tokio::runtime::Runtime, mock_server: &MockServer, item_id: &str) -> Vec<serde_json::Value> {
        runtime
            .block_on(mock_server.received_requests())
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "PATCH" && r.url.path() == format!("/api/me/progress/{item_id}"))
            .map(|r| r.body_json().unwrap())
            .collect()
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Pausing again without the position
    /// having moved must not push the same value again: re-pushing an unchanged, long-paused
    /// position drags the server back over progress made since on another device.
    pub(crate) fn run_an_unchanged_position_is_not_pushed_twice(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(3));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 2.5), Duration::from_secs(10));

        controller.pause();
        pump_until(|| progress_patches(runtime, &mock_server, "item-1").len() == 1, Duration::from_secs(5));
        assert_eq!(progress_patches(runtime, &mock_server, "item-1").len(), 1, "the pause should push once");

        controller.pause();
        controller.pause();
        pump_until(|| false, Duration::from_millis(500));
        assert_eq!(progress_patches(runtime, &mock_server, "item-1").len(), 1, "pausing again at the same position must not push again");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Resuming a book that sat paused
    /// while another device moved it on must continue from the newer position — not from the
    /// old one, which would then be pushed over it — and Undo must put this device's back.
    pub(crate) fn run_resuming_after_a_long_pause_adopts_newer_server_progress(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        let adopted: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));
        controller.set_on_position_adopted({
            let adopted = adopted.clone();
            move |from, to| *adopted.borrow_mut() = Some((from, to))
        });
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(3));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 2.5), Duration::from_secs(10));
        controller.pause();
        pump_until(|| progress_patches(runtime, &mock_server, "item-1").len() == 1, Duration::from_secs(5));

        // Meanwhile, another device listened on to 15s.
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraryItemId": "item-1",
                    "currentTime": 15.0,
                    "duration": 20.0,
                    "isFinished": false,
                    "lastUpdate": chrono::Utc::now().timestamp_millis() + 60_000,
                })))
                .with_priority(1)
                .mount(&mock_server),
        );
        controller.set_resume_reconcile_after(Duration::ZERO);
        controller.play();
        pump_until(|| adopted.borrow().is_some(), Duration::from_secs(10));
        let (from, to) = adopted.borrow().expect("resuming should adopt the newer server position");
        assert!((from - 3.0).abs() < 0.5 && to == 15.0, "adopted {from} -> {to}");
        pump_until(|| controller.snapshot().is_some_and(|s| (s.position_seconds - 15.0).abs() < 0.5), Duration::from_secs(5));
        assert!((controller.snapshot().unwrap().position_seconds - 15.0).abs() < 0.5, "playback should continue from the newer position");
        let pushes_of_old_position =
            progress_patches(runtime, &mock_server, "item-1").iter().filter(|body| (body["currentTime"].as_f64().unwrap() - 3.0).abs() < 0.5).count();
        assert_eq!(pushes_of_old_position, 1, "only the pause pushed the old position; resuming must not push it over the newer one");

        // Undo: back to this device's position, pushed even though the server had it before.
        let patches_before = progress_patches(runtime, &mock_server, "item-1").len();
        controller.seek_to_seconds(from);
        controller.save_progress_now();
        pump_until(|| progress_patches(runtime, &mock_server, "item-1").len() > patches_before, Duration::from_secs(5));
        let last = progress_patches(runtime, &mock_server, "item-1").last().cloned().unwrap();
        assert!((last["currentTime"].as_f64().unwrap() - from).abs() < 0.5, "Undo must push this device's position back: {last}");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A seek made while paused must be
    /// saved (and pushed) once the seeking stops — not only at the next play/pause, which may
    /// never come before the app is closed.
    pub(crate) fn run_a_seek_while_paused_is_saved(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        controller.pause();
        pump_until(|| progress_patches(runtime, &mock_server, "item-1").len() == 1, Duration::from_secs(5));

        // Scrubbing: several seeks in a row, then nothing.
        controller.seek_to_seconds(8.0);
        controller.seek_to_seconds(10.0);
        controller.seek_to_seconds(12.0);
        let saved = || runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap().current_time_seconds;
        pump_until(|| (saved() - 12.0).abs() < 0.5, PAUSED_SEEK_WRITE_DELAY + Duration::from_secs(3));
        assert!((saved() - 12.0).abs() < 0.5, "a seek while paused must be saved, got {}", saved());
        pump_until(|| progress_patches(runtime, &mock_server, "item-1").len() == 2, Duration::from_secs(3));
        let patches = progress_patches(runtime, &mock_server, "item-1");
        assert_eq!(patches.len(), 2, "one push for the whole scrub, not one per seek: {patches:?}");
        assert!((patches[1]["currentTime"].as_f64().unwrap() - 12.0).abs() < 0.5);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Quitting mid-playback must save the
    /// position reached, not the one from the last periodic write up to 5 s earlier.
    pub(crate) fn run_shutdown_flushes_the_final_position(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(4));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 3.5), Duration::from_secs(5));
        controller.stop();

        // No pumping from here on: at shutdown nothing else gets to run.
        controller.downgrade().upgrade().expect("still alive").flush_on_shutdown();
        let saved = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert!((saved.current_time_seconds - 4.0).abs() < 0.5, "shutdown must save the final position, got {}", saved.current_time_seconds);
        assert!(!saved.needs_push, "and push it, when the server is reachable");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A stream cut off mid-file can end
    /// like a finished one. Taken at face value, that marks the book finished from the middle;
    /// it must instead read as a lost connection, keeping the position, with Retry picking up
    /// there. A second early end at the same spot is taken as the real end, so a wrong duration
    /// estimate can't trap playback in a loop.
    pub(crate) fn run_a_stream_that_ends_early_is_not_the_end_of_the_book(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/api/items/item-1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "media": { "audioFiles": [{ "ino": "1", "duration": 600.0 }] }
                })))
                .mount(&mock_server)
                .await;
            Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server).await;
        });
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(100));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 99.5), Duration::from_secs(5));

        // The automatic reload is covered by its own test; this one is about what follows it.
        controller.use_up_auto_recovery();
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::EndOfStream);
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(5));
        let snapshot = controller.snapshot().unwrap();
        assert!(snapshot.last_error.is_some(), "an end 500 s short of the track must read as a lost connection");
        assert!(!snapshot.is_playing);
        assert!((snapshot.position_seconds - 100.0).abs() < 0.5, "the position must be kept, got {}", snapshot.position_seconds);
        pump_until(|| false, Duration::from_millis(300));
        let saved = || runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert!(!saved().is_finished, "the book must not be marked finished");
        assert!((saved().current_time_seconds - 100.0).abs() < 0.5);

        // Retry picks up at 100 s; ending early at the same spot again is taken as the real end.
        controller.play();
        pump_until(|| state.borrow().seek_calls.last() == Some(&Duration::from_secs(100)), Duration::from_secs(5));
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing && s.last_error.is_none()), Duration::from_secs(5));
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::EndOfStream);
        pump_until(|| saved().is_finished, Duration::from_secs(5));
        assert!(saved().is_finished, "a second early end at the same spot is accepted as the end");
        controller.stop();
    }

    #[test]
    fn resume_position_keeps_an_unfinished_position_past_the_reported_end() {
        assert_eq!(resume_position(0.0, false, 100.0), None);
        assert_eq!(resume_position(40.0, false, 100.0), Some(40.0));
        assert_eq!(resume_position(40.0, true, 100.0), None, "a finished book starts over");
        assert_eq!(resume_position(150.0, false, 100.0), Some(99.0), "past the end is clamped inside, not dropped");
        assert_eq!(resume_position(150.0, false, 0.0), Some(150.0), "an unknown duration keeps the saved position");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A download that finishes while the
    /// same track is already streaming (started before, or mid-way through, the download) must
    /// take over at the next seek rather than the stream continuing to be sought indefinitely —
    /// see this plan's section E and the Librem 5 field report ("download next 10 chapters" had
    /// completed, yet the pipeline was still streaming the file it had already loaded).
    pub(crate) fn run_finished_download_takes_over_at_the_next_seek(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        let seconds = 10u32;
        runtime.block_on(mock_playable_item(&mock_server, "item-1", seconds));

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = PlayerController::new(pool.clone(), paths.clone(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        // Confirms it really did start out streaming, not local from the very first load.
        let requests_before = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(requests_before.iter().any(|r| r.url.path() == "/api/items/item-1/file/1"), "playback should start out streamed");

        // The download completes while the stream is still the one loaded — `seed_track_metadata`
        // is needed first since the download-tracks table has a foreign key on the tracks rows,
        // normally written by the sync path this test skips.
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", f64::from(seconds), 0.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(seconds)));

        runtime.block_on(mock_server.reset());
        // No file mock at all now — if the seek below re-requests the stream instead of noticing
        // the finished download, GStreamer's HTTP source will fail outright (connection refused
        // territory) rather than silently succeeding, so a regression here fails loudly.
        controller.skip(2.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 3.0), Duration::from_secs(10));

        let requests_after = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(
            !requests_after.iter().any(|r| r.url.path() == "/api/items/item-1/file/1"),
            "once a download has completed, a seek must switch to the local file rather than issuing another network request for the stream: {requests_after:?}"
        );
        assert!(controller.snapshot().unwrap().last_error.is_none(), "the local-file takeover must not surface an error");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Book-level progress saved by another
    /// client (or an earlier session) can fall inside a later file: resuming at 6s of a 5s+5s
    /// book must load the second file directly at its in-track offset — and, the part the old
    /// first-file-only behavior got wrong, must not fetch the first file at all.
    pub(crate) fn run_resume_jumps_straight_to_the_second_track(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_two_track_item(&mock_server, 5));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 6.0, false)).unwrap();

        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Multi-track Book".to_string(), author: None },
            1.0,
        );

        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 5.9), Duration::from_secs(15));
        let position = controller.snapshot().unwrap().position_seconds;
        assert!(position <= 7.0, "resuming should land near the saved book position, got {position}");

        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(
            !requests.iter().any(|r| r.method.as_str() == "GET" && r.url.path() == "/api/items/item-1/file/1"),
            "resuming into the second track must not fetch the first file"
        );
        assert!(
            requests.iter().any(|r| r.method.as_str() == "GET" && r.url.path() == "/api/items/item-1/file/2"),
            "resuming into the second track should fetch the second file"
        );
        controller.stop();
    }

    pub(crate) fn run_end_of_stream_pauses_and_marks_finished(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        // A very short clip so end-of-stream arrives quickly.
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 1));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let seen: Rc<RefCell<Vec<PlayerSnapshot>>> = Rc::new(RefCell::new(Vec::new()));
        let controller = PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), {
            let seen = seen.clone();
            move |snapshot: &PlayerSnapshot| seen.borrow_mut().push(snapshot.clone())
        });

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Short Clip".to_string(), author: None },
            1.0,
        );

        pump_until(|| seen.borrow().last().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        pump_until(|| seen.borrow().last().is_some_and(|s| !s.is_loading && !s.is_playing), Duration::from_secs(10));

        assert!(
            !seen.borrow().last().unwrap().is_playing,
            "reaching end-of-stream should leave playback paused"
        );

        pump_until(|| false, Duration::from_millis(300));
        let progress =
            runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap();
        assert!(
            progress.is_some_and(|p| p.is_finished),
            "reaching end-of-stream should persist progress marked finished"
        );
        controller.stop();
    }

    /// Pre-caches a cover the way the cover pipeline would have: real PNG bytes on disk under
    /// `covers/<server>/<item>.<ext>`, path recorded on the item row — what
    /// `abs_core::covers::cached_cover_path` then hands the player as its seed.
    async fn seed_cached_cover(
        paths: &abs_storage::AppPaths,
        pool: &SqlitePool,
        server_id: &str,
        item_id: &str,
        extension: &str,
    ) -> std::path::PathBuf {
        let path = paths.cover_cache_path(server_id, item_id, extension);
        tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&path, PNG_1X1).await.unwrap();
        abs_storage::repo::items::set_cover_cache_path(pool, server_id, item_id, Some(&path.to_string_lossy())).await.unwrap();
        path
    }

    /// The reported bug: the player started from `cover_path: None` and relied on the detached
    /// cover-fetch task — whose result could only land in an *existing* matching `now_playing`,
    /// so a cache hit was discarded on a first play (blank cover for the whole session) and on a
    /// re-play it landed in the previous session's struct only to be wiped by `start()`'s tail
    /// (the cover appeared, then vanished). The snapshot must instead be seeded from the local
    /// cover cache immediately, and a failed cover fetch (here: the endpoint 404s) must leave
    /// that cached cover in place.
    pub(crate) fn run_start_shows_the_cached_cover_and_keeps_it_when_the_fetch_fails(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 3));
        // Deliberately no `/api/items/item-1/cover` route: every cover request 404s.

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        let cached_cover = runtime.block_on(seed_cached_cover(&paths, &pool, &server.id, "item-1", "png"));

        let seen: Rc<RefCell<Vec<PlayerSnapshot>>> = Rc::new(RefCell::new(Vec::new()));
        let controller = PlayerController::new(pool.clone(), paths.clone(), test_backend(), {
            let seen = seen.clone();
            move |snapshot: &PlayerSnapshot| seen.borrow_mut().push(snapshot.clone())
        });

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );

        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        assert!(
            seen.borrow().iter().any(|s| s.cover_path.as_deref() == Some(cached_cover.as_path())),
            "snapshots must carry the cached cover from the first publish on, with no network involved"
        );

        // Pump well past where the cover fetch's 404 would have landed: the cached cover must
        // not just appear but *stay* (the old bug blanked it again after the fetch settled).
        pump_until(|| false, Duration::from_millis(500));
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(
            snapshot.cover_path.as_deref(),
            Some(cached_cover.as_path()),
            "a failed cover fetch must leave the cached cover in place"
        );
        controller.stop();
    }

    /// The replace half of the contract: with nothing cached yet, the snapshot starts without a
    /// cover and the detached fetch swaps it over exactly once a valid image has been downloaded
    /// and recorded. (A replace over an *existing* cached cover can't be driven through this
    /// pipeline — a cache hit skips HTTP entirely — which is exactly the "keep using the cached
    /// cover" posture the scenario above pins.)
    pub(crate) fn run_start_replaces_the_cover_once_a_valid_new_one_is_downloaded(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 3));
        runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/api/items/item-1/cover"))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "image/png").set_body_bytes(PNG_1X1.to_vec()))
                .mount(&mock_server)
                .await;
        });

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        let fetched_cover = paths.cover_cache_path(&server.id, "item-1", "png");

        let seen: Rc<RefCell<Vec<PlayerSnapshot>>> = Rc::new(RefCell::new(Vec::new()));
        let controller = PlayerController::new(pool.clone(), paths.clone(), test_backend(), {
            let seen = seen.clone();
            move |snapshot: &PlayerSnapshot| seen.borrow_mut().push(snapshot.clone())
        });

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );

        pump_until(
            || controller.snapshot().is_some_and(|s| s.cover_path.as_deref() == Some(fetched_cover.as_path())),
            Duration::from_secs(10),
        );
        {
            let seen = seen.borrow();
            assert!(seen.first().is_some_and(|s| s.cover_path.is_none()), "with nothing cached, the first snapshot has no cover");
            assert!(
                seen.iter().any(|s| s.cover_path.as_deref() == Some(fetched_cover.as_path())),
                "once a valid cover lands, snapshots must switch to it"
            );
        }
        let item = runtime.block_on(abs_storage::repo::items::get(&pool, &server.id, "item-1")).unwrap();
        assert_eq!(
            item.cover_cache_path.as_deref(),
            Some(fetched_cover.to_string_lossy().as_ref()),
            "the fetch must record the new cover path on the item row"
        );
        controller.stop();
    }

    /// The mini-player bar's own widget wiring (`build_mini_bar`) is otherwise untested by every
    /// scenario above, which all drive a bare `PlayerController` directly. Covers: hidden until
    /// something plays, then visible with the right title/author, a live progress fraction, and
    /// the play/pause button toggling real controller state.
    pub(crate) fn run_mini_bar_reflects_playback_state(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 3));
        // No cover route: the seeded cache below is what must render, and the 404 must not
        // blank it again.

        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        let cached_cover = runtime.block_on(seed_cached_cover(&paths, &pool, &server.id, "item-1", "png"));

        let mini_bar = build_mini_bar(pool.clone(), paths, test_backend());
        let hooks = &mini_bar.hooks;
        assert!(!hooks.bar.is_visible(), "the mini bar should stay hidden until something plays");

        mini_bar.controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: Some("Some Author".to_string()) },
            1.0,
        );
        pump_until(|| hooks.bar.is_visible(), Duration::from_secs(10));

        assert_eq!(hooks.title_label.label(), "Test Book");
        assert_eq!(hooks.author_label.label(), "Some Author");
        pump_until(|| hooks.cover_picture.is_visible(), Duration::from_secs(10));
        assert!(
            hooks.cover_picture.is_visible(),
            "the seeded cached cover must render in the mini bar even though the cover fetch 404s"
        );
        assert!(mini_bar.controller.snapshot().unwrap().cover_path.as_deref() == Some(cached_cover.as_path()));

        pump_until(|| hooks.progress.fraction() > 0.0, Duration::from_secs(5));
        assert!(hooks.progress.fraction() > 0.0, "the progress line should reflect a live position");

        hooks.play_button.emit_clicked();
        pump_until(|| !mini_bar.controller.snapshot().unwrap().is_playing, Duration::from_secs(5));
        assert!(
            !mini_bar.controller.snapshot().unwrap().is_playing,
            "the mini bar's play/pause button should control the real controller"
        );
        mini_bar.controller.stop();
    }

    /// The mini bar's warning glyph is the persistent, at-a-glance indicator of the same
    /// `last_error` the Full Player screen's banner explains in full — `FailingBackend` (not a
    /// real pipeline failure) is used here since this test is only about the glyph's visibility
    /// toggling correctly, which the end-to-end banner test (`screens::player::tests`) doesn't
    /// cover on its own.
    pub(crate) fn run_mini_bar_shows_a_warning_glyph_on_playback_error(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 3));

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let mini_bar = build_mini_bar(pool.clone(), crate::test_support::test_paths(), Box::new(FailingBackend::default()));
        let hooks = &mini_bar.hooks;
        assert!(!hooks.error_icon.is_visible(), "no error before anything has been attempted");

        mini_bar.controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| hooks.error_icon.is_visible(), Duration::from_secs(10));
        assert!(hooks.error_icon.is_visible(), "a failed load must show the mini bar's warning glyph");

        mini_bar.controller.stop();
    }

    /// Serves `item_id` with one audio file per entry of `track_seconds` (optionally answering the
    /// item lookup only after `resolve_delay`, to hold a start in its resolving state) and accepts
    /// progress pushes for it.
    async fn mock_item(mock_server: &MockServer, item_id: &str, track_seconds: &[u32], chapters: &[(f64, f64)], resolve_delay: Option<Duration>) {
        let audio_files: Vec<_> =
            track_seconds.iter().enumerate().map(|(i, secs)| serde_json::json!({ "ino": (i + 1).to_string(), "duration": f64::from(*secs) })).collect();
        let chapters: Vec<_> = chapters
            .iter()
            .enumerate()
            .map(|(i, (start, end))| serde_json::json!({ "id": i, "start": start, "end": end, "title": format!("Chapter {}", i + 1) }))
            .collect();
        let mut response =
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "media": { "audioFiles": audio_files, "chapters": chapters } }));
        if let Some(delay) = resolve_delay {
            response = response.set_delay(delay);
        }
        Mock::given(method("GET")).and(path(format!("/api/items/{item_id}"))).respond_with(response).mount(mock_server).await;
        for (i, secs) in track_seconds.iter().enumerate() {
            Mock::given(method("GET"))
                .and(path(format!("/api/items/{item_id}/file/{}", i + 1)))
                .respond_with(ranged_response(silent_wav_bytes(*secs)))
                .mount(mock_server)
                .await;
        }
        Mock::given(method("PATCH")).and(path(format!("/api/me/progress/{item_id}"))).respond_with(ResponseTemplate::new(200)).mount(mock_server).await;
    }

    fn scripted_controller(pool: &SqlitePool) -> (PlayerController, Rc<RefCell<ScriptedBackendState>>) {
        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        (controller, state)
    }

    fn request(item_id: &str, title: &str) -> PlayRequest {
        PlayRequest { item_id: item_id.to_string(), title: title.to_string(), author: None }
    }

    fn is_loaded(controller: &PlayerController) -> bool {
        controller.snapshot().is_some_and(|s| !s.is_loading)
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The Librem 5 field report: switched
    /// books, pressed play, rewound — and the *old* book played under the new book's cover, at the
    /// old book's position. Starting a book must drop the old one at once: nothing asked of the
    /// old book (a cross-track seek already on its way) or of the player while the new one
    /// resolves (rewind, seek, play) may load or seek anything, and the new book loads exactly
    /// once, at its own position.
    pub(crate) fn run_switching_books_never_plays_the_old_book(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20, 20], &[], None));
        runtime.block_on(mock_item(&mock_server, "item-2", &[30], &[], Some(Duration::from_millis(800))));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Book Two"));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-2", 10.0, false)).unwrap();
        let (controller, state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);

        controller.start(session.clone(), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(15));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 14.5), Duration::from_secs(5));

        // A rewind into the old book's other file, and the new book started right behind it —
        // before the old book's load has had a chance to run.
        controller.seek_to_seconds(25.0);
        controller.start(session.clone(), request("item-2", "Book Two"), 1.0);
        let loads_at_switch = state.borrow().load_calls.len();
        let seeks_at_switch = state.borrow().seek_calls.len();
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.title, "Book Two", "the new book is shown from the moment it's started");
        assert!(snapshot.is_loading && !snapshot.is_playing && snapshot.position_seconds == 0.0);
        assert_eq!(controller.current_item_id().as_deref(), Some("item-2"), "the book that is starting is the current one, never the old one");

        // Transport while it resolves acts on nothing.
        pump_until(|| false, Duration::from_millis(200));
        controller.skip(-15.0);
        controller.seek_to_seconds(5.0);
        controller.play();
        assert_eq!(state.borrow().load_calls.len(), loads_at_switch, "nothing is loaded while the new book resolves");
        assert_eq!(state.borrow().seek_calls.len(), seeks_at_switch, "nothing is seeked while the new book resolves");

        pump_until(|| is_loaded(&controller), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(500));
        let loads: Vec<String> = state.borrow().load_calls[loads_at_switch..].to_vec();
        assert_eq!(loads.len(), 1, "the new book loads exactly once: {loads:?}");
        assert!(loads[0].contains("/api/items/item-2/file/1"), "only the new book's file may load: {loads:?}");
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.title, "Book Two");
        assert!(snapshot.is_playing, "play asked for while loading is honored");
        assert!((snapshot.position_seconds - 10.0).abs() < 0.5, "the new book resumes at its own position, got {}", snapshot.position_seconds);

        let progress = |item_id: &str| {
            runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, item_id)).unwrap().unwrap().current_time_seconds
        };
        assert!((progress("item-1") - 25.0).abs() < 0.5, "the old book keeps the position it was left at, got {}", progress("item-1"));
        assert!((progress("item-2") - 10.0).abs() < 0.5, "the new book's position is untouched, got {}", progress("item-2"));
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A pause while a book is still
    /// resolving — the user's, or an unplug's — holds once it's loaded; it used to start playing
    /// regardless, through the speaker after an unplug.
    pub(crate) fn run_a_pause_while_a_book_loads_holds(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20], &[], Some(Duration::from_millis(500))));
        runtime.block_on(mock_item(&mock_server, "item-2", &[20], &[], Some(Duration::from_millis(500))));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Book Two"));
        let (controller, state) = scripted_controller(&pool);
        controller.set_headphone_behavior(true, true);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);

        controller.start(session.clone(), request("item-1", "Book One"), 1.0);
        controller.toggle_play_pause();
        assert!(controller.snapshot().is_some_and(|s| s.is_loading && !s.will_play_when_loaded && !s.shows_pause_button()));
        pump_until(|| is_loaded(&controller), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(300));
        assert!(!controller.snapshot().unwrap().is_playing, "paused while loading stays paused once loaded");
        assert_eq!(state.borrow().play_calls, 0, "the backend is never asked to play");

        controller.start(session.clone(), request("item-2", "Book Two"), 1.0);
        assert!(controller.snapshot().is_some_and(|s| s.shows_pause_button()), "starting a book means it will play");
        controller.handle_route_event(abs_player::route_watch::RouteEvent::Unplugged);
        pump_until(|| is_loaded(&controller), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(300));
        assert!(!controller.snapshot().unwrap().is_playing, "an unplug while loading keeps it paused once loaded");
        assert_eq!(state.borrow().play_calls, 0);
        // It was the unplug's pause, so a replug resumes it.
        controller.handle_route_event(abs_player::route_watch::RouteEvent::Replugged);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(5));
        assert!(controller.snapshot().unwrap().is_playing);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Of two starts, the later one wins
    /// however the resolves finish — including a slower *failing* start, which used to reset the
    /// backend and replace the book that had loaded since.
    pub(crate) fn run_the_latest_start_wins(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20], &[], Some(Duration::from_millis(1200))));
        runtime.block_on(mock_item(&mock_server, "item-2", &[20], &[], None));
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/items/item-3"))
                .respond_with(ResponseTemplate::new(500).set_delay(Duration::from_millis(1200)))
                .mount(&mock_server),
        );
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        for (id, title) in [("item-1", "Book One"), ("item-2", "Book Two"), ("item-3", "Book Three")] {
            runtime.block_on(insert_synced_item(&pool, &server.id, id, title));
        }
        let (controller, state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);

        controller.start(session.clone(), request("item-1", "Book One"), 1.0);
        controller.start(session.clone(), request("item-2", "Book Two"), 1.0);
        pump_until(|| is_loaded(&controller), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(1800));
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.title, "Book Two");
        assert!(snapshot.is_playing && snapshot.last_error.is_none());
        assert!(state.borrow().load_calls.iter().all(|uri| !uri.contains("item-1")), "the superseded start loads nothing: {:?}", state.borrow().load_calls);

        controller.start(session.clone(), request("item-3", "Book Three"), 1.0);
        controller.start(session.clone(), request("item-2", "Book Two"), 1.0);
        pump_until(|| is_loaded(&controller), Duration::from_secs(10));
        let resets = state.borrow().reset_calls;
        pump_until(|| false, Duration::from_millis(1800));
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.title, "Book Two", "a superseded start's failure must not replace the book loaded since");
        assert!(snapshot.is_playing && snapshot.last_error.is_none());
        assert_eq!(state.borrow().reset_calls, resets, "nor release its pipeline");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A book whose start failed before
    /// anything resolved has no tracks; seeking it used to index an empty track list and crash
    /// the app (the scrubber, skip buttons and MPRIS all reach it).
    pub(crate) fn run_controls_on_a_book_that_failed_to_start_do_not_crash(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(Mock::given(method("GET")).and(path("/api/items/item-1")).respond_with(ResponseTemplate::new(500)).mount(&mock_server));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(10));

        controller.skip(-15.0);
        controller.skip(30.0);
        controller.seek_to_seconds(100.0);
        controller.set_speed(1.5);
        controller.set_sleep_timer_end_of_chapter();
        controller.reset_progress();
        controller.mark_as_finished();
        pump_until(|| false, Duration::from_millis(300));
        assert!(controller.snapshot().unwrap().last_error.is_some(), "still showing why it couldn't start");
        assert!(state.borrow().seek_calls.is_empty());
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A stream error while a track load is
    /// waiting for its pipeline used to be overridden by that load finishing: it cleared the error
    /// and asked the released pipeline to play, restarting the file from its start.
    pub(crate) fn run_an_error_during_a_track_load_is_not_overridden(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20, 20], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        state.borrow_mut().hold_preroll = true;
        controller.seek_to_seconds(25.0);
        let loaded_second = || state.borrow().load_calls.iter().any(|uri| uri.contains("/file/2"));
        pump_until(loaded_second, Duration::from_secs(5));
        assert!(loaded_second());
        // The automatic reload is covered by its own test; this one is about what follows it.
        controller.use_up_auto_recovery();
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::Error(abs_player::PlaybackError {
            kind: abs_player::PlaybackErrorKind::Network,
            message: "simulated network failure".to_string(),
            debug: None,
        }));
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(5));
        let plays = state.borrow().play_calls;
        // The pipeline "prerolls" now — the abandoned load must not pick that up.
        state.borrow_mut().position = Some(Duration::ZERO);
        pump_until(|| false, Duration::from_millis(800));
        let snapshot = controller.snapshot().unwrap();
        assert!(snapshot.last_error.is_some() && !snapshot.is_playing, "the error stands until the user retries");
        assert_eq!(state.borrow().play_calls, plays, "the abandoned load must not resume the released pipeline");
        assert!((snapshot.position_seconds - 25.0).abs() < 0.5, "the position is where the load was headed, got {}", snapshot.position_seconds);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A speed change and a seek made while
    /// a track is loading are the ones applied once it's ready — the load used to re-apply the
    /// speed it captured when it began, and to seek to its own original target.
    pub(crate) fn run_changes_during_a_track_load_are_applied_by_it(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20, 20], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        state.borrow_mut().hold_preroll = true;
        controller.seek_to_seconds(25.0);
        let loaded_second = || state.borrow().load_calls.iter().any(|uri| uri.contains("/file/2"));
        pump_until(loaded_second, Duration::from_secs(5));
        controller.set_speed(1.5);
        controller.seek_to_seconds(32.0);
        assert!(state.borrow().speed_calls.is_empty(), "nothing is applied to a pipeline that isn't ready");
        state.borrow_mut().position = Some(Duration::ZERO);
        pump_until(|| !state.borrow().speed_calls.is_empty(), Duration::from_secs(5));
        assert_eq!(state.borrow().speed_calls.last().copied(), Some((1.5, Duration::from_secs(12))), "the latest speed, at the latest target");
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.speed, 1.5);
        assert!((snapshot.position_seconds - 32.0).abs() < 0.5, "got {}", snapshot.position_seconds);
        assert!(snapshot.is_playing);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A pause's confirmation must not take
    /// a load that followed it for a pause that never landed — it used to flag "the audio pipeline
    /// never actually paused" and release the freshly loaded pipeline.
    pub(crate) fn run_a_load_after_a_pause_is_not_a_stuck_pause(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20, 20], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        // From here on the pipeline never reports Paused — after the load below, that is simply
        // because it was replaced, not because the pause got stuck.
        state.borrow_mut().stuck_paused = true;
        controller.pause();
        controller.seek_to_seconds(25.0);
        pump_until(|| false, PAUSE_CONFIRM_INTERVAL * (PAUSE_CONFIRM_ATTEMPTS + 2));
        let snapshot = controller.snapshot().unwrap();
        assert!(snapshot.last_error.is_none(), "a load after a pause is not a stuck pause: {:?}", snapshot.last_error);
        assert!((snapshot.position_seconds - 25.0).abs() < 0.5, "got {}", snapshot.position_seconds);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Home and Library pull the server's
    /// progress into the local row on their own; by the time a long-paused book is resumed, the
    /// row may already hold another device's newer position. The resume used to adopt only a
    /// value that changed during its own check, so it carried on from the stale position and
    /// pushed it over the newer one.
    pub(crate) fn run_resuming_adopts_newer_progress_already_pulled_locally(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        let adopted: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));
        controller.set_on_position_adopted({
            let adopted = adopted.clone();
            move |from, to| *adopted.borrow_mut() = Some((from, to))
        });
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(3));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 2.5), Duration::from_secs(5));
        controller.pause();
        pump_until(|| progress_patches(runtime, &mock_server, "item-1").len() == 1, Duration::from_secs(5));

        // Another device listened on to 15s, and Home has already pulled that in.
        let last_update = chrono::Utc::now().timestamp_millis() + 60_000;
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraryItemId": "item-1", "currentTime": 15.0, "duration": 20.0, "isFinished": false, "lastUpdate": last_update,
                })))
                .with_priority(1)
                .mount(&mock_server),
        );
        let connection = abs_core::connection::ConnectionTarget::direct(&mock_server.uri());
        runtime
            .block_on(abs_core::progress_sync::reconcile_item_progress(&pool, &connection, &account.token, &account.id, &server.id, "item-1"))
            .unwrap();
        let local = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert_eq!(local.current_time_seconds, 15.0, "precondition: the local row already holds the newer position");

        controller.set_resume_reconcile_after(Duration::ZERO);
        controller.play();
        pump_until(|| adopted.borrow().is_some(), Duration::from_secs(10));
        let (from, to) = adopted.borrow().expect("resuming should adopt the newer position already in the local row");
        assert!((from - 3.0).abs() < 0.5 && to == 15.0, "adopted {from} -> {to}");
        let pushes_of_old_position =
            progress_patches(runtime, &mock_server, "item-1").iter().filter(|body| (body["currentTime"].as_f64().unwrap() - 3.0).abs() < 0.5).count();
        assert_eq!(pushes_of_old_position, 1, "only the pause pushed the old position");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Starting at a tapped chapter loads
    /// the book straight at the chapter (the saved position doesn't win). On the book already
    /// loaded it is just a seek, with no reload; on the book already starting, the latest tap wins.
    pub(crate) fn run_starting_at_a_chapter(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[60], &[(0.0, 20.0), (20.0, 40.0), (40.0, 60.0)], None));
        runtime.block_on(mock_item(&mock_server, "item-2", &[60], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Book Two"));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 50.0, false)).unwrap();
        let (controller, state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);

        controller.start_with(session.clone(), request("item-1", "Book One"), 1.0, Some(1));
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        assert_eq!(state.borrow().seek_calls.first().copied(), Some(Duration::from_secs(20)), "loaded straight at the chapter");
        assert!((controller.snapshot().unwrap().position_seconds - 20.0).abs() < 0.5);

        let loads = state.borrow().load_calls.len();
        controller.start_with(session.clone(), request("item-1", "Book One"), 1.0, Some(2));
        pump_until(|| controller.snapshot().is_some_and(|s| (s.position_seconds - 40.0).abs() < 0.5), Duration::from_secs(5));
        assert!((controller.snapshot().unwrap().position_seconds - 40.0).abs() < 0.5, "got {}", controller.snapshot().unwrap().position_seconds);
        assert_eq!(state.borrow().load_calls.len(), loads, "a chapter of the loaded book is a seek, not a reload");
        assert!(controller.snapshot().unwrap().is_playing);

        // Tapped twice while it's still starting: the latest chapter is where it starts.
        controller.start(session.clone(), request("item-2", "Book Two"), 1.0);
        pump_until(|| is_loaded(&controller), Duration::from_secs(10));
        let loads = state.borrow().load_calls.len();
        controller.start_with(session.clone(), request("item-1", "Book One"), 1.0, Some(0));
        controller.start_with(session.clone(), request("item-1", "Book One"), 1.0, Some(1));
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading && s.is_playing), Duration::from_secs(10));
        assert_eq!(state.borrow().load_calls.len(), loads + 1, "one start, one load");
        assert!((controller.snapshot().unwrap().position_seconds - 20.0).abs() < 0.5, "got {}", controller.snapshot().unwrap().position_seconds);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Play or Resume on the book that's
    /// already loaded used to stop it and fetch it from the server again — a gap, a loading
    /// state, and a long wait offline. It now just plays, from where it is.
    pub(crate) fn run_starting_the_loaded_book_does_not_reload_it(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[60], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        controller.start(session.clone(), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(30));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 29.5), Duration::from_secs(5));
        controller.pause();
        let (loads, resets) = (state.borrow().load_calls.len(), state.borrow().reset_calls);

        controller.start(session.clone(), request("item-1", "Book One"), 1.0);
        let snapshot = controller.snapshot().unwrap();
        assert!(!snapshot.is_loading && snapshot.is_playing, "it plays at once, without loading again");
        pump_until(|| false, Duration::from_millis(500));
        assert_eq!(state.borrow().load_calls.len(), loads, "no reload");
        assert_eq!(state.borrow().reset_calls, resets, "the pipeline is kept");
        assert!((controller.snapshot().unwrap().position_seconds - 30.0).abs() < 0.5, "got {}", controller.snapshot().unwrap().position_seconds);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A finished book used to come back
    /// unfinished: every later write (a pause, a quit, switching books) recorded it unfinished at
    /// wherever the pipeline stopped, here and on the server. And Play on it resumed the pipeline
    /// at its end, which only ended again — it now starts over.
    pub(crate) fn run_a_finished_book_stays_finished(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[60], &[], None));
        runtime.block_on(mock_item(&mock_server, "item-2", &[60], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Book Two"));
        let (controller, state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let row = |item_id: &str| runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, item_id)).unwrap();
        let finished = |item_id: &str| row(item_id).is_some_and(|r| r.is_finished);

        // Listened to the end.
        controller.start(session.clone(), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(60));
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::EndOfStream);
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_playing), Duration::from_secs(5));
        pump_until(|| finished("item-1"), Duration::from_secs(5));
        // A pause (MPRIS sends them whatever the state) and the switch to another book.
        controller.pause();
        controller.start(session.clone(), request("item-2", "Book Two"), 1.0);
        pump_until(|| is_loaded(&controller), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(500));
        assert!(finished("item-1"), "a pause or a switch after the end keeps it finished");

        // Marked finished by hand, then quit.
        state.borrow_mut().position = Some(Duration::from_secs(20));
        pump_until(|| false, Duration::from_millis(300));
        controller.mark_as_finished();
        controller.flush_on_shutdown();
        let marked = row("item-2").unwrap();
        assert!(marked.is_finished, "quitting after marking it finished keeps it finished");
        assert!((marked.current_time_seconds - 60.0).abs() < 0.5, "got {}", marked.current_time_seconds);

        // Play on a finished book starts it over, with a fresh load of its first file.
        let loads = state.borrow().load_calls.len();
        controller.play();
        pump_until(|| state.borrow().load_calls.len() > loads && controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(5));
        assert_eq!(state.borrow().load_calls.len(), loads + 1, "a finished book is loaded again from its start");
        assert!(controller.snapshot().unwrap().position_seconds < 0.5, "got {}", controller.snapshot().unwrap().position_seconds);
        pump_until(|| !finished("item-2"), Duration::from_secs(5));
        assert!(!finished("item-2"), "started over, it's no longer finished");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Pause, then rewind: the seek makes
    /// the pipeline preroll again, which on a stream lasts as long as the new range request. The
    /// pause confirmation used to read that as a pause that never landed — "the audio pipeline
    /// never actually paused" — and release the pipeline.
    pub(crate) fn run_a_seek_right_after_a_pause_is_not_a_stuck_pause(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(15));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 14.5), Duration::from_secs(5));
        let resets = state.borrow().reset_calls;

        controller.pause();
        controller.skip(-10.0);
        // The re-preroll after that seek never finishes within the confirmation window.
        state.borrow_mut().stuck_paused = true;
        pump_until(|| false, PAUSE_CONFIRM_INTERVAL * (PAUSE_CONFIRM_ATTEMPTS + 2));
        let snapshot = controller.snapshot().unwrap();
        assert!(snapshot.last_error.is_none(), "a seek after a pause is not a stuck pause: {:?}", snapshot.last_error);
        assert_eq!(state.borrow().reset_calls, resets, "the paused pipeline is kept");
        assert!((snapshot.position_seconds - 5.0).abs() < 0.5, "got {}", snapshot.position_seconds);

        // A speed picked while paused on a stream isn't handed to the pipeline until Play, so it
        // can't disturb the pause either.
        state.borrow_mut().stuck_paused = false;
        controller.play();
        controller.pause();
        controller.set_speed(1.5);
        pump_until(|| false, PAUSE_CONFIRM_INTERVAL * (PAUSE_CONFIRM_ATTEMPTS + 2));
        assert!(controller.snapshot().unwrap().last_error.is_none());
        assert_eq!(state.borrow().reset_calls, resets);
        assert!(state.borrow().speed_calls.is_empty());
        controller.stop();
    }


    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Dragging the full player's scrubber
    /// used to seek at every value it passed through — on a stream, a new range request each
    /// time. Only the value it settles on is sought now, once; and dragging it all the way right
    /// stops short of the end instead of finishing the book.
    pub(crate) fn run_a_scrubber_drag_seeks_once(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[100], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        let download_manager = crate::downloads::DownloadManager::new(
            pool.clone(),
            crate::test_support::test_paths(),
            Box::new(abs_player::network_watch::UnknownNetworkMonitor),
            false,
        );
        let screen = crate::screens::player::build(pool.clone(), controller.clone(), download_manager, || {}, || {});
        let hooks = screen.test_hooks();
        let seeks = state.borrow().seek_calls.len();

        for fraction in [0.1, 0.15, 0.2, 0.25, 0.3] {
            hooks.scrubber.set_value(fraction);
        }
        assert_eq!(hooks.elapsed_label.label(), "0:30", "the time follows the knob while it moves");
        pump_until(|| false, Duration::from_millis(600));
        let sought: Vec<Duration> = state.borrow().seek_calls[seeks..].to_vec();
        assert_eq!(sought.len(), 1, "one seek per drag: {sought:?}");
        assert!((sought[0].as_secs_f64() - 30.0).abs() < 0.01, "to where the knob settled: {sought:?}");
        assert!((hooks.scrubber.value() - 0.3).abs() < 0.02, "the knob stays where it was left, got {}", hooks.scrubber.value());

        hooks.scrubber.set_value(1.0);
        pump_until(|| false, Duration::from_millis(600));
        let snapshot = controller.snapshot().unwrap();
        assert!((snapshot.position_seconds - 99.0).abs() < 0.5, "a seek to the end stops short of it, got {}", snapshot.position_seconds);
        pump_until(|| false, Duration::from_millis(600));
        assert!(controller.snapshot().unwrap().is_playing, "the book plays on rather than finishing");
        controller.stop();
    }


    fn network_error() -> abs_player::PlayerEvent {
        abs_player::PlayerEvent::Error(abs_player::PlaybackError {
            kind: abs_player::PlaybackErrorKind::Network,
            message: "simulated network failure".to_string(),
            debug: None,
        })
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A stream error while playing — a
    /// dropped connection, an expired token — used to stop with an error until the listener
    /// tapped Retry, which only reloads the file. It now reloads once by itself, at the same
    /// position, and keeps playing; a second error soon after stops with the error as before.
    pub(crate) fn run_a_stream_error_reloads_once_by_itself(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[60], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(30));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 29.5), Duration::from_secs(5));
        let loads = state.borrow().load_calls.len();

        state.borrow_mut().pending_event = Some(network_error());
        pump_until(|| state.borrow().load_calls.len() > loads && state.borrow().seek_calls.last() == Some(&Duration::from_secs(30)), Duration::from_secs(5));
        pump_until(|| false, Duration::from_millis(300));
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(state.borrow().load_calls.len(), loads + 1, "reloaded once");
        assert!(snapshot.last_error.is_none() && snapshot.is_playing, "still playing, no error to tap through");
        assert!((snapshot.position_seconds - 30.0).abs() < 0.5, "at the same position, got {}", snapshot.position_seconds);

        state.borrow_mut().pending_event = Some(network_error());
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(5));
        let snapshot = controller.snapshot().unwrap();
        assert!(snapshot.last_error.is_some() && !snapshot.is_playing, "a second error soon after stops with the error");
        assert_eq!(state.borrow().load_calls.len(), loads + 1, "and doesn't reload again");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The tick doesn't run while paused,
    /// so a stream error from then (burst buffering keeps downloading) used to surface only
    /// right after Play — "play, then it stops". Play now finds it and resumes with a reload. And
    /// after a long pause a stream resumes with a fresh connection rather than the paused one,
    /// which may have died while the phone slept.
    pub(crate) fn run_resuming_a_stream_reloads_when_its_connection_is_gone(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[60], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(30));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 29.5), Duration::from_secs(5));
        controller.pause();
        // Let the tick notice the pause and stop, so nothing polls the bus any more.
        pump_until(|| false, Duration::from_millis(600));

        // An error arrived while paused.
        state.borrow_mut().pending_event = Some(network_error());
        let loads = state.borrow().load_calls.len();
        controller.play();
        pump_until(|| state.borrow().load_calls.len() > loads && state.borrow().seek_calls.last() == Some(&Duration::from_secs(30)), Duration::from_secs(5));
        pump_until(|| false, Duration::from_millis(300));
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(state.borrow().load_calls.len(), loads + 1, "play reloads instead of resuming a failed stream");
        assert!(snapshot.last_error.is_none() && snapshot.is_playing);
        assert!((snapshot.position_seconds - 30.0).abs() < 0.5, "got {}", snapshot.position_seconds);

        // A long pause: the stream is loaded afresh.
        controller.pause();
        pump_until(|| false, Duration::from_millis(600));
        controller.set_stale_connection_after(Duration::from_millis(100));
        let loads = state.borrow().load_calls.len();
        controller.play();
        pump_until(|| state.borrow().load_calls.len() > loads, Duration::from_secs(5));
        pump_until(|| false, Duration::from_millis(300));
        assert_eq!(state.borrow().load_calls.len(), loads + 1, "a long-paused stream resumes with a fresh load");
        assert!((controller.snapshot().unwrap().position_seconds - 30.0).abs() < 0.5);

        // A short pause just resumes.
        controller.set_stale_connection_after(STALE_CONNECTION_AFTER);
        controller.pause();
        let loads = state.borrow().load_calls.len();
        controller.play();
        pump_until(|| false, Duration::from_millis(300));
        assert_eq!(state.borrow().load_calls.len(), loads, "a short pause resumes the same pipeline");
        controller.stop();
    }


    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A resume seek that never landed was
    /// given up on by following the pipeline's own position — the start of the file — which the
    /// next write saved and pushed over the listener's real position. The file is now loaded again
    /// at the target once; if that fails too, playback stops with an error and the target stays
    /// the saved position. A target past the file's real end lands at that end, though.
    pub(crate) fn run_a_seek_that_never_lands_reloads_once_then_stops(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[60], &[], None));
        runtime.block_on(mock_item(&mock_server, "item-2", &[60], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Book Two"));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 15.0, false)).unwrap();
        let (controller, state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let saved = |item_id: &str| runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, item_id)).unwrap().unwrap();

        state.borrow_mut().seeks_to_ignore = u32::MAX;
        controller.start(session.clone(), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(30));
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(snapshot.last_error.as_ref().map(|e| e.kind), Some(abs_player::PlaybackErrorKind::Seek));
        assert!(!snapshot.is_playing);
        assert_eq!(state.borrow().load_calls.len(), 2, "loaded once more at the target, then stopped: {:?}", state.borrow().load_calls);
        assert!((snapshot.position_seconds - 15.0).abs() < 0.5, "the target is still the position, got {}", snapshot.position_seconds);
        controller.pause();
        pump_until(|| false, Duration::from_millis(300));
        assert!((saved("item-1").current_time_seconds - 15.0).abs() < 0.5, "never saved anywhere else, got {}", saved("item-1").current_time_seconds);

        // The scripted file is really 20 s long: a seek to 50 s stops at its end, and that counts.
        state.borrow_mut().seeks_to_ignore = 0;
        controller.start(session.clone(), request("item-2", "Book Two"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading && s.is_playing), Duration::from_secs(10));
        state.borrow_mut().seeks_to_ignore = u32::MAX;
        state.borrow_mut().position = Some(Duration::from_secs(20));
        let loads = state.borrow().load_calls.len();
        controller.seek_to_seconds(50.0);
        pump_until(|| false, SEEK_REISSUE_AFTER * u32::from(MAX_SEEK_REISSUES + 2));
        let snapshot = controller.snapshot().unwrap();
        assert!(snapshot.last_error.is_none() && snapshot.is_playing, "{:?}", snapshot.last_error);
        assert_eq!(state.borrow().load_calls.len(), loads, "no reload for a seek clamped at the file's end");
        assert!((snapshot.position_seconds - 20.0).abs() < 0.5, "got {}", snapshot.position_seconds);
        controller.stop();
    }


    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. An end-of-stream on the last file
    /// was taken as the end of the book anywhere within 10% of that file — the last hour of a
    /// 10-hour single-file book, from a truncated download or a stream cut short. It now pauses
    /// there without marking the book finished; Play loads it again at that spot, and ending at
    /// the same spot again is the real end.
    pub(crate) fn run_an_early_end_of_the_last_file_does_not_finish_the_book(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[2000], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        let saved = || runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        // 100 s short of a 2000 s file: more than a minute, less than 10%.
        state.borrow_mut().position = Some(Duration::from_secs(1900));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 1899.5), Duration::from_secs(5));
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::EndOfStream);
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_playing), Duration::from_secs(5));
        pump_until(|| false, Duration::from_millis(300));
        let snapshot = controller.snapshot().unwrap();
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        assert!((snapshot.position_seconds - 1900.0).abs() < 0.5, "got {}", snapshot.position_seconds);
        assert!(!saved().is_finished, "not finished on an unclear end");
        assert!((saved().current_time_seconds - 1900.0).abs() < 0.5, "got {}", saved().current_time_seconds);

        let loads = state.borrow().load_calls.len();
        controller.play();
        pump_until(|| state.borrow().seek_calls.last() == Some(&Duration::from_secs(1900)), Duration::from_secs(5));
        assert_eq!(state.borrow().load_calls.len(), loads + 1, "play loads it again at that spot");
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(5));
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::EndOfStream);
        pump_until(|| saved().is_finished, Duration::from_secs(5));
        assert!(saved().is_finished, "the same end again is the real end");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. An end-of-stream already waiting on
    /// the bus when the listener rewinds is about the position they left: it used to be judged
    /// against the new one and swallow the rewind — moving on to the next file, or finishing the
    /// book.
    pub(crate) fn run_an_end_of_stream_from_before_a_seek_is_dropped(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[60, 60], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(59));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 58.5), Duration::from_secs(5));
        let loads = state.borrow().load_calls.len();

        // The file ends, and before the next tick sees it, the listener skips back.
        state.borrow_mut().pending_event = Some(abs_player::PlayerEvent::EndOfStream);
        controller.skip(-30.0);
        pump_until(|| false, Duration::from_millis(800));
        let snapshot = controller.snapshot().unwrap();
        assert_eq!(state.borrow().load_calls.len(), loads, "still in the first file");
        assert!(snapshot.is_playing);
        assert!((snapshot.position_seconds - 29.0).abs() < 0.5, "got {}", snapshot.position_seconds);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The outgoing book's final position is
    /// saved even while a resume check (which holds the ordinary writes back) is still waiting on
    /// a slow server: it used to be dropped.
    pub(crate) fn run_switching_books_during_a_resume_check_still_saves_the_outgoing_position(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        runtime.block_on(mock_playable_item(&mock_server, "item-2", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "First Book"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Second Book"));

        let state = Rc::new(RefCell::new(ScriptedBackendState::default()));
        let controller =
            PlayerController::new(pool.clone(), crate::test_support::test_paths(), Box::new(ScriptedBackend(state.clone())), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "First Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(3));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 2.5), Duration::from_secs(10));
        controller.pause();
        pump_until(|| progress_patches(runtime, &mock_server, "item-1").len() == 1, Duration::from_secs(5));

        // The resume check's server answer is slow, so the hold is still in place when the
        // user moves on to the other book.
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({
                            "libraryItemId": "item-1",
                            "currentTime": 3.0,
                            "duration": 20.0,
                            "isFinished": false,
                            "lastUpdate": 1,
                        }))
                        .set_delay(Duration::from_secs(3)),
                )
                .with_priority(1)
                .mount(&mock_server),
        );
        controller.set_resume_reconcile_after(Duration::ZERO);
        controller.play();
        state.borrow_mut().position = Some(Duration::from_secs(9));
        pump_until(|| false, Duration::from_millis(400));

        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-2".to_string(), title: "Second Book".to_string(), author: None },
            1.0,
        );
        let saved = || {
            runtime
                .block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1"))
                .ok()
                .flatten()
                .is_some_and(|p| (p.current_time_seconds - 9.0).abs() < 0.5)
        };
        pump_until(saved, Duration::from_secs(5));
        assert!(saved(), "the outgoing book's position must be saved even while its resume check is still in flight");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A push that hangs on the server must
    /// not hold up the local writes behind it (they used to share one queue), and a push still
    /// starts only after its own local write.
    pub(crate) fn run_a_hanging_push_does_not_delay_local_writes(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("PATCH"))
                .and(path("/api/me/progress/item-1"))
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(8)))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("PATCH")).and(path("/api/me/progress/item-2")).respond_with(ResponseTemplate::new(200)).mount(&mock_server),
        );
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "First Book"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Second Book"));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let write = |item_id: &str, position: f64| ProgressWrite {
            pool: pool.clone(),
            session: session.clone(),
            account_id: account.id.clone(),
            server_id: server.id.clone(),
            item_id: item_id.to_string(),
            position,
            is_finished: false,
            duration_seconds: 100.0,
            write_local: true,
            push: true,
            on_progress_sync: None,
        };

        let writer = ProgressWriter::default();
        writer.enqueue(write("item-1", 10.0));
        pump_until(|| writer.in_flight_push.borrow().is_some(), Duration::from_secs(5));
        assert!(writer.in_flight_push.borrow().is_some(), "the first push should be in flight (and hanging)");

        let started = Instant::now();
        writer.enqueue(write("item-2", 20.0));
        let row = || runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-2")).unwrap();
        pump_until(|| row().is_some(), Duration::from_secs(5));
        assert_eq!(row().expect("the later local write should not wait for the hanging push").current_time_seconds, 20.0);
        assert!(started.elapsed() < Duration::from_secs(4), "the local write took {:?}", started.elapsed());

        // The hanging push is cut short at shutdown, not waited for in full.
        let started = Instant::now();
        writer.drain_blocking(Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_secs(3), "the drain took {:?}", started.elapsed());
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A speed picked while paused on a
    /// stream is shown at once but only handed to the backend (a range request) at Play; a
    /// repeated pick and an unusable value do nothing.
    pub(crate) fn run_a_speed_picked_while_paused_on_a_stream_is_applied_at_play(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[60], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        controller.pause();
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_playing), Duration::from_secs(5));

        controller.set_speed(2.0);
        assert_eq!(controller.snapshot().unwrap().speed, 2.0, "the pick is shown right away");
        assert!(state.borrow().speed_calls.is_empty(), "but the backend isn't asked while paused on a stream");

        controller.set_speed(f64::NAN);
        controller.set_speed(f64::INFINITY);
        controller.set_speed(2.0);
        assert!(state.borrow().speed_calls.is_empty());
        assert_eq!(controller.snapshot().unwrap().speed, 2.0);

        controller.play();
        assert_eq!(state.borrow().speed_calls.len(), 1, "Play applies it, once");
        assert_eq!(state.borrow().speed_calls[0].0, 2.0);

        // Picked and put back while paused: the backend never hears of it.
        controller.pause();
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_playing), Duration::from_secs(5));
        controller.set_speed(1.5);
        controller.set_speed(2.0);
        controller.play();
        assert_eq!(state.borrow().speed_calls.len(), 1, "no net change, nothing to apply");

        // Out of range is brought into range.
        controller.set_speed(10.0);
        assert_eq!(controller.snapshot().unwrap().speed, abs_core::playback::MAX_SPEED);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A retired controller (its shell was
    /// replaced) saves the position, stops its audio, tells its listeners nothing more, and
    /// ignores everything that arrives for it afterwards.
    pub(crate) fn run_a_retired_player_goes_silent_and_stays_that_way(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        let (controller, state) = scripted_controller(&pool);
        let heard = Rc::new(std::cell::Cell::new(0_u32));
        controller.add_listener({
            let heard = heard.clone();
            move |_| heard.set(heard.get() + 1)
        });
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Test Item"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(7));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 6.5), Duration::from_secs(5));

        let resets_before = state.borrow().reset_calls;
        controller.retire();
        assert!(state.borrow().reset_calls > resets_before, "the backend is released");
        assert!(controller.snapshot().is_none(), "nothing is loaded any more");
        let heard_at_retirement = heard.get();

        // Late arrivals: media-key presses and a stale start's network answer.
        controller.play();
        controller.skip(30.0);
        pump_until(|| false, Duration::from_millis(800));
        assert_eq!(heard.get(), heard_at_retirement, "no listener hears from a retired controller");
        assert!(controller.snapshot().is_none());
        let saved = || {
            runtime
                .block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1"))
                .ok()
                .flatten()
                .is_some_and(|p| (p.current_time_seconds - 7.0).abs() < 0.6)
        };
        pump_until(saved, Duration::from_secs(5));
        assert!(saved(), "the position at retirement is saved");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. While a book is still starting it is
    /// already "the current item" (so Item Detail routes its Reset/Mark finished to the
    /// controller, and a Player opened then has a download context), and such an action is applied
    /// once the book is loaded instead of being lost or overwritten by the start.
    pub(crate) fn run_progress_actions_during_a_start_apply_once_loaded(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[20], &[], Some(Duration::from_millis(800))));
        runtime.block_on(mock_item(&mock_server, "item-2", &[20], &[], Some(Duration::from_millis(800))));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "Book Two"));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 12.0, false)).unwrap();
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-2", 12.0, false)).unwrap();
        let (controller, _state) = scripted_controller(&pool);

        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        assert!(controller.snapshot().is_some_and(|s| s.is_loading));
        assert_eq!(controller.current_item_id().as_deref(), Some("item-1"), "a book that is starting is the current one");
        let (_, server_id, item_id) = controller.current_download_context().expect("a context while starting");
        assert_eq!((server_id.as_str(), item_id.as_str()), (server.id.as_str(), "item-1"));

        controller.mark_as_finished();
        assert!(controller.snapshot().is_some_and(|s| s.is_loading), "asking doesn't end the start");
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        assert!(!controller.snapshot().unwrap().is_playing, "marked finished: not playing");
        let finished = || {
            runtime
                .block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1"))
                .ok()
                .flatten()
                .is_some_and(|p| p.is_finished && (p.current_time_seconds - 20.0).abs() < 0.5)
        };
        pump_until(finished, Duration::from_secs(5));
        assert!(finished(), "the finished mark reaches the saved progress (it was 12s, unfinished)");

        // And a reset asked for during a start.
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-2", "Book Two"), 1.0);
        assert!(controller.snapshot().is_some_and(|s| s.is_loading));
        controller.reset_progress();
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        let reset = || {
            runtime
                .block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-2"))
                .ok()
                .flatten()
                .is_some_and(|p| !p.is_finished && p.current_time_seconds < 0.5)
        };
        pump_until(reset, Duration::from_secs(5));
        assert!(reset(), "the reset reaches the saved progress");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A file that runs longer than the
    /// server said stalls the reported position at its end instead of showing the next file's
    /// stretch of the book early.
    pub(crate) fn run_the_position_never_runs_past_its_file_in_the_server_timeline(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[10, 10], &[], None));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Book One"));
        let (controller, state) = scripted_controller(&pool);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Book One"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        state.borrow_mut().position = Some(Duration::from_secs(14));
        pump_until(|| false, Duration::from_millis(600));
        let position = controller.snapshot().unwrap().position_seconds;
        assert!(position <= 10.0, "still in the first file, which the server says ends at 10s: got {position}");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A listener tied to a screen that has
    /// gone is dropped by the publish that finds out, and so is a mini bar built for a screen
    /// that is then discarded (Item Detail builds one per visit; each used to stay registered,
    /// updating widgets nobody could see, for good).
    pub(crate) fn run_listeners_of_discarded_screens_are_dropped(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        let (controller, _state) = scripted_controller(&pool);
        let permanent_before = controller.listener_count();
        let permanent_calls = Rc::new(std::cell::Cell::new(0_u32));
        controller.add_listener({
            let permanent_calls = permanent_calls.clone();
            move |_| permanent_calls.set(permanent_calls.get() + 1)
        });
        let scoped_calls = Rc::new(std::cell::Cell::new(0_u32));
        controller.add_scoped_listener({
            let scoped_calls = scoped_calls.clone();
            move |_| {
                scoped_calls.set(scoped_calls.get() + 1);
                scoped_calls.get() < 2
            }
        });
        let bar = build_mini_bar_for(controller.clone());
        assert_eq!(controller.listener_count(), permanent_before + 3);

        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Test Item"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(600));
        assert_eq!(scoped_calls.get(), 2, "told to stop after its second call, never called again");
        assert_eq!(controller.listener_count(), permanent_before + 2, "the scoped listener is gone, the mini bar's is still there");

        drop(bar);
        pump_until(|| false, Duration::from_millis(600));
        assert_eq!(controller.listener_count(), permanent_before + 1, "the discarded screen's mini bar listener is gone");
        let calls = permanent_calls.get();
        pump_until(|| false, Duration::from_millis(600));
        assert!(permanent_calls.get() > calls, "permanent listeners keep hearing");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A bouncing headphone jack (replugged,
    /// then unplugged again within moments) is not a reconnection and must not resume playback
    /// (which would play out of the speaker the moment it bounced back); a replug that holds does.
    pub(crate) fn run_a_bouncing_headphone_jack_does_not_resume(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        let (controller, _state) = scripted_controller(&pool);
        controller.set_headphone_behavior(true, true);
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Test Item"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        use abs_player::route_watch::RouteEvent;
        controller.handle_route_event(RouteEvent::Unplugged);
        assert!(!controller.snapshot().unwrap().is_playing, "an unplug pauses at once");
        controller.handle_route_event(RouteEvent::Replugged);
        controller.handle_route_event(RouteEvent::Unplugged);
        pump_until(|| false, REPLUG_SETTLE * 3);
        assert!(!controller.snapshot().unwrap().is_playing, "the jack bounced; nothing may resume");

        controller.handle_route_event(RouteEvent::Replugged);
        assert!(!controller.snapshot().unwrap().is_playing, "not before the plug has held");
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(5));
        assert!(controller.snapshot().unwrap().is_playing, "a replug that holds resumes");
        controller.stop();
    }

    /// A server that accepts the connection and then never answers — the "Wi-Fi connected but
    /// dead" case — for the start-from-the-files scenarios below.
    async fn mock_hanging_server(mock_server: &MockServer) {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(mock_server)
            .await;
        Mock::given(method("PATCH")).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60))).mount(mock_server).await;
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A book whose files are on the device
    /// starts from them after a few seconds even if the server never answers — it used to wait for
    /// the full HTTP timeout (15 s) first.
    pub(crate) fn run_a_downloaded_book_starts_without_waiting_for_a_dead_server(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_hanging_server(&mock_server));
        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 3.0, 0.0), ("2", 2.0, 3.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(3)));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "2", &silent_wav_bytes(2)));

        let (controller, state) = scripted_controller(&pool);
        let started = Instant::now();
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Offline Book"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        let waited = started.elapsed();
        assert!(controller.snapshot().is_some_and(|s| s.is_playing && s.last_error.is_none()), "it plays");
        assert!(waited < LOCAL_START_SERVER_WAIT + Duration::from_secs(2), "started after {waited:?}; the dead server must not be waited for in full");
        assert!(waited >= LOCAL_START_SERVER_WAIT - Duration::from_millis(500), "a server that might still answer is given its short chance: {waited:?}");
        assert!(state.borrow().load_calls.iter().all(|uri| uri.starts_with("file://")), "only the downloaded files are loaded: {:?}", state.borrow().load_calls);
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The same, with an access token that
    /// expired while the app was closed: deciding that the book can start from the device must not
    /// wait for the token refresh, which on a dead connection takes the full HTTP timeout. On the
    /// phone this held a downloaded book up for ~40 s.
    pub(crate) fn run_a_downloaded_book_starts_without_waiting_for_a_token_refresh(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_hanging_server(&mock_server));
        runtime.block_on(
            Mock::given(method("POST"))
                .and(path("/auth/refresh"))
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
                .mount(&mock_server),
        );
        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        // A JWT whose `exp` is long past, plus a refresh token to exchange for a new one.
        let expired = "eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjF9.c2lnbmF0dXJl";
        runtime.block_on(abs_storage::repo::accounts::set_tokens(&pool, &account.id, expired, Some("refresh"))).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account.id)).unwrap();
        assert!(abs_core::auth::jwt_exp_seconds(&account.token).is_some_and(|exp| exp < 60), "the test token must read as expired");
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 3.0, 0.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(3)));

        let (controller, state) = scripted_controller(&pool);
        let started = Instant::now();
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Offline Book"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(30));
        let waited = started.elapsed();
        assert!(controller.snapshot().is_some_and(|s| s.is_playing && s.last_error.is_none()), "it plays");
        assert!(waited < LOCAL_START_SERVER_WAIT + Duration::from_secs(2), "started after {waited:?}; the token refresh must not be waited for");
        assert!(state.borrow().load_calls.iter().all(|uri| uri.starts_with("file://")), "only the downloaded file is loaded: {:?}", state.borrow().load_calls);
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(requests.iter().any(|r| r.url.path() == "/auth/refresh"), "the refresh is still tried, in the background");
        controller.stop();
    }

    /// With offline mode on, the same start doesn't contact the server at all.
    pub(crate) fn run_offline_mode_starts_a_downloaded_book_without_contacting_the_server(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_hanging_server(&mock_server));
        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 3.0, 0.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(3)));

        let (controller, _state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        session.set_offline(true);
        let started = Instant::now();
        controller.start(session, request("item-1", "Offline Book"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        assert!(controller.snapshot().is_some_and(|s| s.is_playing));
        assert!(started.elapsed() < Duration::from_secs(2), "started after {:?}", started.elapsed());
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(
            !requests.iter().any(|r| r.url.path().starts_with("/api/items/")),
            "offline mode: no item request before playing: {:?}",
            requests.iter().map(|r| r.url.path().to_string()).collect::<Vec<_>>()
        );
        controller.stop();
    }

    /// A book whose *start* track isn't on the device still waits for the server as before (it
    /// needs it), then falls back to the cached tracks.
    pub(crate) fn run_a_book_whose_start_track_is_not_downloaded_still_waits_for_the_server(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item(&mock_server, "item-1", &[3, 2], &[], Some(Duration::from_secs(5))));
        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 3.0, 0.0), ("2", 2.0, 3.0)]));
        // Only the second file is on the device; the book starts in the first.
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "2", &silent_wav_bytes(2)));

        let (controller, _state) = scripted_controller(&pool);
        let started = Instant::now();
        controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), request("item-1", "Partial Book"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(15));
        assert!(started.elapsed() >= Duration::from_secs(4), "it waited for the server it needs: {:?}", started.elapsed());
        assert!(controller.snapshot().is_some_and(|s| s.is_playing));
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. With offline mode on, a downloaded book
    /// plays and pauses without a single request reaching the server, its progress is kept locally,
    /// and it's pushed once offline mode is off again.
    pub(crate) fn run_offline_mode_plays_and_saves_locally_then_catches_up(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server),
        );
        runtime.block_on(Mock::given(method("GET")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(404)).mount(&mock_server));
        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 30.0, 0.0)]));
        runtime.block_on(seed_downloaded_track(&pool, &paths, &server.id, "item-1", "1", &silent_wav_bytes(30)));

        let (controller, state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        session.set_offline(true);
        controller.start(session.clone(), request("item-1", "Offline Book"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        state.borrow_mut().position = Some(Duration::from_secs(7));
        pump_until(|| controller.snapshot().is_some_and(|s| s.position_seconds >= 6.5), Duration::from_secs(5));
        controller.pause();
        let saved = || {
            runtime
                .block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1"))
                .ok()
                .flatten()
                .is_some_and(|p| (p.current_time_seconds - 7.0).abs() < 0.5 && p.needs_push)
        };
        pump_until(saved, Duration::from_secs(5));
        assert!(saved(), "the position is kept locally, marked for pushing");
        pump_until(|| false, Duration::from_millis(300));
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(requests.is_empty(), "offline mode: nothing may reach the server, got {:?}", requests.iter().map(|r| r.url.path().to_string()).collect::<Vec<_>>());

        // Offline mode off: what the shell does on that switch.
        session.set_offline(false);
        controller.sync_pending_progress();
        pump_until(|| !progress_patches(runtime, &mock_server, "item-1").is_empty(), Duration::from_secs(5));
        let pushed = progress_patches(runtime, &mock_server, "item-1");
        assert!((pushed.last().unwrap()["currentTime"].as_f64().unwrap() - 7.0).abs() < 0.5, "the kept position is pushed: {pushed:?}");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. With offline mode on, a book whose
    /// start isn't downloaded doesn't start (and says why) instead of reaching for the server.
    pub(crate) fn run_offline_mode_does_not_stream(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 20));
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(seed_track_metadata(&pool, &server.id, "item-1", &[("1", 20.0, 0.0)]));

        let (controller, state) = scripted_controller(&pool);
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        session.set_offline(true);
        controller.start(session, request("item-1", "Streamed Book"), 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        let snapshot = controller.snapshot().unwrap();
        assert!(!snapshot.is_playing);
        assert_eq!(snapshot.last_error.as_ref().map(|e| e.kind), Some(abs_player::PlaybackErrorKind::Offline));
        assert!(state.borrow().load_calls.is_empty(), "nothing is streamed");
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(requests.is_empty(), "offline mode: nothing may reach the server, got {:?}", requests.iter().map(|r| r.url.path().to_string()).collect::<Vec<_>>());
        controller.stop();
    }

    fn three_chapters() -> Vec<ChapterInfo> {
        [("One", 0.0, 100.0), ("Two", 100.0, 250.0), ("Three", 250.0, 400.0)]
            .into_iter()
            .map(|(title, start_seconds, end_seconds)| ChapterInfo { title: title.to_string(), start_seconds, end_seconds })
            .collect()
    }

    #[test]
    fn next_chapter_goes_to_the_following_chapters_start() {
        assert_eq!(next_chapter_start(&three_chapters(), 0.0), Some(100.0));
        assert_eq!(next_chapter_start(&three_chapters(), 120.0), Some(250.0));
    }

    #[test]
    fn next_chapter_in_the_last_chapter_goes_nowhere() {
        assert_eq!(next_chapter_start(&three_chapters(), 300.0), None);
        // Past every range clamps to the last chapter, which has no next one either.
        assert_eq!(next_chapter_start(&three_chapters(), 400.0), None);
    }

    #[test]
    fn previous_chapter_well_into_a_chapter_restarts_it() {
        assert_eq!(previous_chapter_start(&three_chapters(), 180.0), Some(100.0));
        assert_eq!(previous_chapter_start(&three_chapters(), 100.0 + PREVIOUS_CHAPTER_RESTART_SECONDS + 0.5), Some(100.0));
    }

    #[test]
    fn previous_chapter_at_a_chapters_start_goes_to_the_one_before() {
        assert_eq!(previous_chapter_start(&three_chapters(), 101.0), Some(0.0));
        assert_eq!(previous_chapter_start(&three_chapters(), 100.0 + PREVIOUS_CHAPTER_RESTART_SECONDS), Some(0.0));
        assert_eq!(previous_chapter_start(&three_chapters(), 250.0), Some(100.0));
    }

    #[test]
    fn previous_chapter_in_the_first_chapter_goes_to_its_start() {
        assert_eq!(previous_chapter_start(&three_chapters(), 1.0), Some(0.0));
        assert_eq!(previous_chapter_start(&three_chapters(), 50.0), Some(0.0));
    }

    #[test]
    fn a_position_before_the_first_chapter_is_in_the_first_chapter() {
        let chapters: Vec<ChapterInfo> = [("One", 2.0, 100.0), ("Two", 100.0, 200.0)]
            .into_iter()
            .map(|(title, start_seconds, end_seconds)| ChapterInfo { title: title.to_string(), start_seconds, end_seconds })
            .collect();
        assert_eq!(chapter_index_at(&chapters, 1.0), Some(0));
        assert_eq!(chapter_index_at(&three_chapters(), 120.0), Some(1));
        assert_eq!(chapter_index_at(&three_chapters(), 500.0), Some(2), "past the end is the last chapter");
        assert_eq!(next_chapter_start(&chapters, 1.0), Some(100.0));
    }

    #[test]
    fn chapter_jumps_without_chapters_go_nowhere() {
        assert_eq!(next_chapter_start(&[], 10.0), None);
        assert_eq!(previous_chapter_start(&[], 10.0), None);
    }
}
