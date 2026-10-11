//! The download pipeline's core: mapping chapter selections onto tracks, computing offline
//! availability, and the resumable single-track fetch itself. See `repo::download_tracks`/
//! `repo::tracks` (`abs-storage`) for why tracking is per-track, not per-chapter — the server only
//! ever serves whole audio files.
//!
//! `download_track` is the one function in this module that does real network I/O; everything
//! else here is pure logic over already-fetched/cached data. Deliberately does **not** own a
//! queue, a concurrency limit, or a registry of in-flight cancellation flags — that orchestration
//! belongs to the app-layer `DownloadManager` (`app/src/downloads.rs`), which calls this function
//! once per track it decides to run, the same way `app/src/player.rs`'s `PlayerController` is the
//! one place that drives `abs_player::AudioBackend`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use sqlx::SqlitePool;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use abs_storage::models::{DownloadStatus, DownloadTrack};
use abs_storage::AppPaths;

use crate::error::Result;
use crate::tracks::TrackRef;

/// Generous absolute ceiling on one *attempt* of a track download — not the per-chunk stall
/// detector below, which is what actually catches a hung connection in practice. This just bounds
/// the pathological case of a connection that keeps trickling single bytes forever without ever
/// going fully idle.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// A chunk read that takes longer than this is treated as a stalled connection — retried (with
/// backoff, resuming from whatever was already written) rather than left to hang indefinitely.
const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Retries are for transient conditions (a dropped connection, a `5xx`); a genuinely broken
/// request (bad auth, a track that no longer exists) will never succeed no matter how many times
/// it's retried, so this cap exists to eventually give up and surface a real error rather than
/// loop forever against a server that will never say yes.
const MAX_ATTEMPTS: u32 = 5;
const BACKOFF_BASE: Duration = Duration::from_millis(500);
/// How often `bytes_downloaded` is persisted to the database mid-transfer — bounded so a fast
/// local network doesn't turn every single chunk into its own `UPDATE`, while still keeping the
/// resumable checkpoint reasonably fresh if the app is killed mid-download.
const PROGRESS_PERSIST_INTERVAL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackDownloadOutcome {
    Completed,
    Canceled,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineAvailability {
    None,
    Partial,
    Full,
}

/// Which track holds a given book-level position — same `rposition`-based semantics as
/// `streaming::locate_track`, just over `TrackRef` (no URL) instead of `StreamTrack`. Kept as a
/// separate small function rather than converting to `StreamTrack` and back: the two types exist
/// for different reasons (one carries a playable URL, one doesn't), and forcing one through the
/// other's shape for a single index lookup isn't worth it.
fn track_index_at(tracks: &[TrackRef], book_seconds: f64) -> usize {
    tracks.iter().rposition(|t| t.offset_seconds <= book_seconds + 1e-6).unwrap_or(0)
}

/// The track containing the instant *just before* `book_seconds` — used for a chapter's `end`
/// boundary (chapters are `[start, end)`, half-open). Deliberately a strict `<`, not `track_index_at`
/// with a subtracted epsilon: subtracting a small epsilon from `end` and then adding one back
/// inside `track_index_at`'s own fuzz can round-trip back onto the boundary itself in floating
/// point, which put a chapter's needed tracks one track too far in testing. A plain strict `<`
/// has no such cancellation.
fn track_index_before(tracks: &[TrackRef], book_seconds: f64) -> usize {
    tracks.iter().rposition(|t| t.offset_seconds < book_seconds).unwrap_or(0)
}

/// Which `ino`s the given chapter indices touch. Maps each chapter's book-level `[start, end)`
/// range onto the tracks it overlaps: the start boundary resolves normally, and the end boundary
/// is probed just before `end` (not `end` itself) so a chapter's own last track is included
/// without spilling onto whatever track the *next* chapter's first instant belongs to.
pub fn tracks_needed_for_chapters(tracks: &[TrackRef], chapters: &[abs_api::ChapterRef], chapter_indices: &[usize]) -> BTreeSet<String> {
    let mut needed = BTreeSet::new();
    if tracks.is_empty() {
        return needed;
    }
    for &index in chapter_indices {
        let Some(chapter) = chapters.get(index) else { continue };
        let start_idx = track_index_at(tracks, chapter.start_seconds);
        let end_idx = track_index_before(tracks, chapter.end_seconds).max(start_idx);
        for track in &tracks[start_idx..=end_idx] {
            needed.insert(track.ino.clone());
        }
    }
    needed
}

/// The total bytes a download for the given chapter indices would need to fetch — the sum of the
/// server-reported sizes of every *distinct* track those chapters touch. Whole files are what
/// downloads fetch, so a chapter straddling two tracks counts both, and two chapters sharing one
/// file count it once. `None` when any touched track's size is unknown — callers show no estimate
/// rather than a wrong one. Pure (no I/O), for the download sheet's per-row subtitles; takes
/// plain `(f64, f64)` chapter ranges so a caller with its own chapter type (the Player screen's
/// `ChapterInfo`) never has to construct an `abs_api` one, the same boundary
/// `chapter_offline_markers_for_item` below already draws.
pub fn estimate_bytes_for_chapters(tracks: &[TrackRef], chapter_ranges: &[(f64, f64)], chapter_indices: &[usize]) -> Option<u64> {
    let chapters: Vec<abs_api::ChapterRef> = chapter_ranges
        .iter()
        .map(|&(start_seconds, end_seconds)| abs_api::ChapterRef { title: String::new(), start_seconds, end_seconds })
        .collect();
    let needed = tracks_needed_for_chapters(tracks, &chapters, chapter_indices);
    // An empty `needed` (no cached tracks at all, or chapters that map onto none) is "can't know",
    // not "zero bytes" — a caller showing "≈0 B" for a real book would be worse than no estimate.
    if needed.is_empty() {
        return None;
    }
    let mut total = 0u64;
    for ino in needed {
        let track = tracks.iter().find(|t| t.ino == ino)?;
        total += track.size_bytes?;
    }
    Some(total)
}

/// One bool per chapter (in order) — the per-chapter offline glyph Item Detail will show, without
/// storing a single "chapter downloaded" row anywhere (there's no such file to point at). A
/// chapter counts as offline only if *every* track it touches is complete — a chapter split across
/// a track boundary where only one side finished downloading is not yet safely playable offline.
pub fn chapter_offline_markers(tracks: &[TrackRef], chapters: &[abs_api::ChapterRef], complete_inos: &BTreeSet<String>) -> Vec<bool> {
    (0..chapters.len())
        .map(|index| {
            let needed = tracks_needed_for_chapters(tracks, chapters, std::slice::from_ref(&index));
            !needed.is_empty() && needed.is_subset(complete_inos)
        })
        .collect()
}

/// Whether an item is fully, partially, or not at all downloaded — compared against its actual
/// (cached) track count, the definition the Home/Library offline-mode toggle filters on. Returns
/// `None` (the availability, not the Rust `Option`) if the item's tracks haven't even been synced
/// locally yet — nothing can be "downloaded" if there's no cached record of what to download.
pub async fn item_offline_availability(pool: &SqlitePool, server_id: &str, item_id: &str) -> Result<OfflineAvailability> {
    let tracks = abs_storage::repo::tracks::list_for_item(pool, server_id, item_id).await?;
    if tracks.is_empty() {
        return Ok(OfflineAvailability::None);
    }
    let downloads = abs_storage::repo::download_tracks::list_for_item(pool, server_id, item_id).await?;
    let complete: BTreeSet<&str> = downloads.iter().filter(|d| d.status == DownloadStatus::Complete).map(|d| d.ino.as_str()).collect();
    let done = tracks.iter().filter(|t| complete.contains(t.ino.as_str())).count();
    Ok(match done {
        0 => OfflineAvailability::None,
        d if d == tracks.len() => OfflineAvailability::Full,
        _ => OfflineAvailability::Partial,
    })
}

/// One-call version of the tracks-fetch + complete-tracks-fetch + `chapter_offline_markers`
/// pipeline, taking chapters as plain `(start_seconds, end_seconds)` pairs rather than
/// `abs_api::ChapterRef` — so a caller (the Player screen) that only has its own in-memory chapter
/// list (`app::player::ChapterInfo`, not an `abs_api` type) never has to construct one just to call
/// this, keeping `app`'s "never imports `abs_api` directly" rule intact.
pub async fn chapter_offline_markers_for_item(pool: &SqlitePool, server_id: &str, item_id: &str, chapter_ranges: &[(f64, f64)]) -> Result<Vec<bool>> {
    let tracks = crate::tracks::cached_tracks(pool, server_id, item_id).await?;
    let complete = complete_inos_for_item(pool, server_id, item_id).await?;
    let chapters: Vec<abs_api::ChapterRef> =
        chapter_ranges.iter().map(|&(start_seconds, end_seconds)| abs_api::ChapterRef { title: String::new(), start_seconds, end_seconds }).collect();
    Ok(chapter_offline_markers(&tracks, &chapters, &complete))
}

/// Why a track is streamed rather than played from disk — the reason `local_track_path` used to
/// swallow. Logged at playback start so "why did it stream a book I downloaded?" is answerable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamReason {
    /// No download was ever started for this track.
    NotDownloaded,
    /// A download row exists but hasn't finished (pending or in progress).
    InProgress,
    /// The download ended in failure.
    Failed,
    /// Marked complete, but the file is gone from disk.
    FileMissing,
    /// Marked complete, but the file's size isn't what was recorded.
    SizeMismatch { expected: u64, actual: u64 },
    /// The download table couldn't be read.
    LookupFailed(String),
}

impl std::fmt::Display for StreamReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamReason::NotDownloaded => write!(f, "not downloaded"),
            StreamReason::InProgress => write!(f, "download not finished"),
            StreamReason::Failed => write!(f, "download failed"),
            StreamReason::FileMissing => write!(f, "downloaded file is missing"),
            StreamReason::SizeMismatch { expected, actual } => write!(f, "downloaded file is {actual} bytes, expected {expected}"),
            StreamReason::LookupFailed(err) => write!(f, "couldn't read the downloads table: {err}"),
        }
    }
}

/// Where a track plays from: a verified local file, or the stream (and why).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSource {
    Local(PathBuf),
    Streamed(StreamReason),
}

/// A `Complete` row is only trustworthy if its file still exists on disk with the size recorded at
/// download time — a size mismatch (or a missing file, e.g. deleted externally) means the row is
/// stale, and callers must treat it the same as "not downloaded" rather than crash or play/report a
/// corrupt file. Shared by `download_track`'s own idempotency check (skip a re-download) and
/// `track_source` below (prefer a local file over streaming) — one definition of "trustworthy",
/// not two copies that could quietly drift apart.
async fn verified_complete_path(row: &DownloadTrack) -> Option<PathBuf> {
    match verify_row(row).await {
        TrackSource::Local(path) => Some(path),
        TrackSource::Streamed(_) => None,
    }
}

async fn verify_row(row: &DownloadTrack) -> TrackSource {
    match row.status {
        DownloadStatus::Complete => {}
        DownloadStatus::Pending | DownloadStatus::Downloading => return TrackSource::Streamed(StreamReason::InProgress),
        DownloadStatus::Failed => return TrackSource::Streamed(StreamReason::Failed),
    }
    // Offline-first: everything here is local (the file, and the size recorded when it finished),
    // and only proof of a problem counts against a download. A file that's definitely gone is
    // missing; one that merely can't be checked right now (a permission or I/O error) is played
    // anyway — refusing a download because of a hiccup would make it unavailable for no reason.
    let metadata = match tokio::fs::metadata(&row.file_path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return TrackSource::Streamed(StreamReason::FileMissing),
        Err(err) => {
            tracing::warn!(%err, path = %row.file_path, "couldn't check a downloaded file; playing it anyway");
            return TrackSource::Local(PathBuf::from(&row.file_path));
        }
    };
    match row.expected_size_bytes {
        Some(expected) if expected as u64 != metadata.len() => {
            TrackSource::Streamed(StreamReason::SizeMismatch { expected: expected as u64, actual: metadata.len() })
        }
        _ => TrackSource::Local(PathBuf::from(&row.file_path)),
    }
}

/// Where this track plays from — the on-disk path if, and only if, it's verifiably safe to play
/// from (a `Complete` row whose file still matches its recorded size), otherwise the stream and
/// the reason. Never fails the caller: a DB read error is a `Streamed(LookupFailed)`, logged.
pub async fn track_source(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str) -> TrackSource {
    match abs_storage::repo::download_tracks::get(pool, server_id, item_id, ino).await {
        Ok(Some(row)) => {
            let source = verify_row(&row).await;
            if let TrackSource::Streamed(reason) = &source {
                if row.status == DownloadStatus::Complete && is_proven_damage(&row, reason).await {
                    mark_damaged(pool, &row, reason).await;
                }
            }
            source
        }
        Ok(None) => TrackSource::Streamed(StreamReason::NotDownloaded),
        Err(err) => {
            tracing::warn!(%err, item_id, ino, "couldn't read the downloads table; streaming this track");
            TrackSource::Streamed(StreamReason::LookupFailed(err.to_string()))
        }
    }
}

/// Whether a finished download's failed check proves the download itself is bad, rather than
/// only that it can't be reached right now. The wrong size is proof. A missing file is proof only
/// when its folder is still there: if the whole folder is gone too (storage not mounted, say),
/// the files may well come back, so nothing is written off.
async fn is_proven_damage(row: &DownloadTrack, reason: &StreamReason) -> bool {
    match reason {
        StreamReason::SizeMismatch { .. } => true,
        StreamReason::FileMissing => match std::path::Path::new(&row.file_path).parent() {
            Some(dir) => tokio::fs::metadata(dir).await.is_ok_and(|m| m.is_dir()),
            None => false,
        },
        _ => false,
    }
}

/// Stops counting a finished download whose file is proven bad (see [`is_proven_damage`]): every
/// "downloaded" marker reads the row's status, so without this the chapter kept looking
/// downloaded while playback refused it — and offline mode then said it "isn't downloaded".
/// Downloading it again starts over from the first byte. Best-effort: a failed write is logged,
/// and the next check tries again.
async fn mark_damaged(pool: &SqlitePool, row: &DownloadTrack, reason: &StreamReason) {
    tracing::warn!(item_id = %row.item_id, ino = %row.ino, %reason, "a downloaded file is damaged; it no longer counts as downloaded");
    let message = format!("the downloaded file was damaged ({reason}) — download it again");
    if let Err(err) = abs_storage::repo::download_tracks::mark_damaged(pool, &row.server_id, &row.item_id, &row.ino, &message).await {
        tracing::warn!(%err, item_id = %row.item_id, ino = %row.ino, "couldn't record the damaged download");
    }
}

/// The on-disk path for a track, if — and only if, see [`track_source`]. `None` covers every other
/// case uniformly, for callers that only need "is there a local file".
pub async fn local_track_path(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str) -> Option<PathBuf> {
    match track_source(pool, server_id, item_id, ino).await {
        TrackSource::Local(path) => Some(path),
        TrackSource::Streamed(_) => None,
    }
}

/// Which of `inos` a download has to fetch: every one that isn't a usable local file by the same
/// check playback makes ([`track_source`]). So asking for a download also re-checks the chapters
/// already marked downloaded: one whose file is gone or the wrong size is written off (see
/// [`mark_damaged`]) and fetched again from the first byte, instead of being reported as done.
/// Only the size can be checked — the server gives no checksum for a file.
pub async fn tracks_to_fetch(pool: &SqlitePool, server_id: &str, item_id: &str, inos: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut to_fetch = Vec::new();
    for ino in inos {
        if !matches!(track_source(pool, server_id, item_id, &ino).await, TrackSource::Local(_)) {
            to_fetch.push(ino);
        }
    }
    to_fetch
}

/// Which of an item's tracks are fully downloaded — what `chapter_offline_markers` needs to turn
/// into per-chapter glyphs, without the Player screen having to reach into `abs_storage` directly
/// (the same boundary `item_offline_availability` above already respects).
pub async fn complete_inos_for_item(pool: &SqlitePool, server_id: &str, item_id: &str) -> Result<BTreeSet<String>> {
    let downloads = abs_storage::repo::download_tracks::list_for_item(pool, server_id, item_id).await?;
    Ok(downloads.into_iter().filter(|d| d.status == DownloadStatus::Complete).map(|d| d.ino).collect())
}

/// Item ids (scoped to one server) downloaded fully or partially — the query behind the
/// Home/Library offline-mode toggle's filtered view. Thin pass-through to the storage repo; kept
/// here (rather than calling the repo directly from `app`) so `app` never has to know the offline
/// definition lives in a "complete tracks" table rather than something chapter-shaped.
pub async fn downloaded_item_ids(pool: &SqlitePool, server_id: &str) -> Result<Vec<String>> {
    Ok(abs_storage::repo::download_tracks::downloaded_item_ids(pool, server_id).await?)
}

/// "Clear downloaded chapters" (the whole item, since tracking is per-track): delete every file on
/// disk (best-effort — a file already missing is not an error, anything else is logged and
/// skipped rather than aborting the rest of the cleanup) and then the database rows.
pub async fn clear_item_downloads(pool: &SqlitePool, server_id: &str, item_id: &str) -> Result<()> {
    let removed = abs_storage::repo::download_tracks::remove_for_item(pool, server_id, item_id).await?;
    for row in removed {
        if let Err(err) = tokio::fs::remove_file(&row.file_path).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%err, item_id, ino = %row.ino, "couldn't delete a downloaded track file");
            }
        }
    }
    Ok(())
}

/// "Clear all downloads" for a server: delete every download-track row and its backing file
/// (same best-effort file cleanup as [`clear_item_downloads`]), returning the distinct item ids
/// that were affected so the caller can tell the UI each of those items is now idle.
pub async fn clear_all_downloads(pool: &SqlitePool, server_id: &str) -> Result<Vec<String>> {
    let removed = abs_storage::repo::download_tracks::remove_for_server(pool, server_id).await?;
    let mut item_ids: Vec<String> = removed.iter().map(|row| row.item_id.clone()).collect();
    item_ids.sort();
    item_ids.dedup();
    for row in removed {
        if let Err(err) = tokio::fs::remove_file(&row.file_path).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%err, item_id = %row.item_id, ino = %row.ino, "couldn't delete a downloaded track file");
            }
        }
    }
    Ok(item_ids)
}

/// A transfer that ended with an unknown total size (`None`) is trusted — there's nothing to
/// verify it against. One with a known size is only complete if every expected byte actually
/// arrived; a connection that closes early has fewer bytes than promised and must not be mistaken
/// for a clean finish.
fn transfer_is_complete(bytes_downloaded: u64, total_size: Option<u64>) -> bool {
    total_size.map(|expected| bytes_downloaded == expected).unwrap_or(true)
}

async fn backoff(attempt: u32) {
    let delay = BACKOFF_BASE * 2u32.pow(attempt.saturating_sub(1)).min(16); // caps at 8s (500ms * 16)
    tokio::time::sleep(delay).await;
}

/// Fetches one track's audio file to disk, resuming from wherever a previous attempt left off.
/// Idempotent: calling this again after a `Completed` outcome (with the file still intact on disk)
/// is a cheap no-op, same posture as `covers::fetch_and_cache_cover`'s cache-hit check.
///
/// `on_progress` is called after each chunk is written (not just at the DB-persist interval) so a
/// UI can show smooth progress even though the database itself is only updated periodically.
/// `cancel` is polled before every attempt and around every chunk read — a cooperative check, not
/// a hard abort, so a cancellation always lands between well-defined units of work (never
/// mid-write of a single chunk).
/// How many times a download's final write (complete, or failed) is tried before giving up.
const FINAL_WRITE_ATTEMPTS: u32 = 3;

/// Saves how far a download has got. Bookkeeping only, so a failure — a database too busy to
/// answer in time, as on a phone whose main loop froze for seconds and left every connection
/// held — is logged and the transfer carries on: the next save records where it got to, and a
/// resume trims the file back to whichever checkpoint was saved last. A failed save used to end
/// the download (and, with the old resume, damage the file).
async fn save_progress(pool: &SqlitePool, server_id: &str, item_id: &str, ino: &str, bytes_downloaded: u64, total_size: Option<i64>) {
    if let Err(err) = abs_storage::repo::download_tracks::update_progress(pool, server_id, item_id, ino, bytes_downloaded as i64, total_size).await {
        tracing::warn!(%err, item_id, ino, bytes_downloaded, "couldn't save download progress; carrying on");
    }
}

/// Retries a download's final write a few times, so a short database stall doesn't throw away a
/// finished (or properly failed) download; only the last failure is returned.
async fn retrying<F, Fut>(mut write: F) -> abs_storage::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = abs_storage::Result<()>>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match write().await {
            Ok(()) => return Ok(()),
            Err(err) if attempt < FINAL_WRITE_ATTEMPTS => {
                tracing::warn!(%err, attempt, "couldn't record the download's outcome; trying again");
                backoff(attempt).await;
            }
            Err(err) => return Err(err),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn download_track(
    paths: &AppPaths,
    pool: &SqlitePool,
    connection: &crate::connection::ConnectionTarget,
    access_token: &str,
    server_id: &str,
    item_id: &str,
    ino: &str,
    mut on_progress: impl FnMut(u64, Option<u64>),
    cancel: &dyn Fn() -> bool,
) -> Result<TrackDownloadOutcome> {
    let existing = abs_storage::repo::download_tracks::get(pool, server_id, item_id, ino).await?;

    if let Some(row) = &existing {
        if verified_complete_path(row).await.is_some() {
            return Ok(TrackDownloadOutcome::Completed);
        }
    }

    if cancel() {
        return Ok(TrackDownloadOutcome::Canceled);
    }

    // A row marked complete got here only because its file failed verification above: its
    // `bytes_downloaded` describes a file that can't be trusted, so start over from the first byte.
    let mut resume_offset = existing.as_ref().filter(|r| r.status != DownloadStatus::Complete).map(|r| r.bytes_downloaded as u64).unwrap_or(0);
    let mut file_path: Option<PathBuf> = existing.map(|r| PathBuf::from(r.file_path));

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if cancel() {
            return Ok(TrackDownloadOutcome::Canceled);
        }

        let client = match connection.api_client_with_timeout(access_token, REQUEST_TIMEOUT) {
            Ok(client) => client,
            Err(err) => {
                // A mint failure with a configured connection is a concrete, user-fixable
                // problem (missing/unreadable client certificate) — reported as the download's
                // failure reason rather than crashing the download worker.
                let reason = format!("couldn't set up the connection: {err}");
                retrying(|| abs_storage::repo::download_tracks::mark_failed(pool, server_id, item_id, ino, &reason)).await?;
                return Ok(TrackDownloadOutcome::Failed(reason));
            }
        };

        // Resuming needs the file to hold at least the saved checkpoint; one that's shorter (or
        // gone) was changed outside this download, so its progress can't be trusted.
        if resume_offset > 0 {
            let on_disk = match &file_path {
                Some(path) => tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0),
                None => 0,
            };
            if on_disk < resume_offset {
                tracing::warn!(item_id, ino, on_disk, resume_offset, "the partial download is shorter than its saved progress; starting it over");
                resume_offset = 0;
            }
        }

        let range = (resume_offset > 0).then_some(resume_offset);
        let file_response = match client.get_item_file_response(item_id, ino, range).await {
            Ok(response) => response,
            Err(err) => {
                if !err.is_retryable() || attempt >= MAX_ATTEMPTS {
                    let reason = err.to_string();
                    retrying(|| abs_storage::repo::download_tracks::mark_failed(pool, server_id, item_id, ino, &reason)).await?;
                    return Ok(TrackDownloadOutcome::Failed(reason));
                }
                backoff(attempt).await;
                continue;
            }
        };

        // The server ignored our Range request and sent the full body from byte 0 — start the
        // file over rather than risk appending a full body onto already-written bytes.
        let fresh_start = range.is_some() && !file_response.resumed;
        if fresh_start {
            resume_offset = 0;
        }

        let path = match &file_path {
            Some(existing_path) if !fresh_start => existing_path.clone(),
            _ => {
                let extension = crate::media_type::extension_for(file_response.content_type.as_deref().unwrap_or(""), "mp3");
                let dir = paths.item_downloads_dir(server_id, item_id);
                if let Err(err) = tokio::fs::create_dir_all(&dir).await {
                    let reason = format!("couldn't create the download directory: {err}");
                    retrying(|| abs_storage::repo::download_tracks::mark_failed(pool, server_id, item_id, ino, &reason)).await?;
                    return Ok(TrackDownloadOutcome::Failed(reason));
                }
                let path = paths.track_file_path(server_id, item_id, ino, extension);
                abs_storage::repo::download_tracks::upsert_pending(pool, server_id, item_id, ino, &path.to_string_lossy()).await?;
                path
            }
        };
        file_path = Some(path.clone());

        // Resume from the saved checkpoint, not from the end of the file. Progress is saved every
        // `PROGRESS_PERSIST_INTERVAL_BYTES`, so a download interrupted between saves leaves more
        // bytes on disk than the checkpoint says — and the server sends from the checkpoint. The
        // file used to be opened for appending, which wrote those bytes a second time (a file
        // 512 KiB too big, refused by `verify_row` on a phone). Cutting it back to the checkpoint
        // also drops a chunk that may only have been half written.
        let resuming = !fresh_start && resume_offset > 0;
        let opened = tokio::fs::OpenOptions::new().create(true).write(true).truncate(!resuming).open(&path).await;
        let mut file = match opened {
            Ok(file) => file,
            Err(err) => {
                let reason = format!("couldn't open the download file: {err}");
                retrying(|| abs_storage::repo::download_tracks::mark_failed(pool, server_id, item_id, ino, &reason)).await?;
                return Ok(TrackDownloadOutcome::Failed(reason));
            }
        };
        if resuming {
            let positioned = async {
                file.set_len(resume_offset).await?;
                file.seek(std::io::SeekFrom::Start(resume_offset)).await
            };
            if let Err(err) = positioned.await {
                let reason = format!("couldn't resume the download file: {err}");
                retrying(|| abs_storage::repo::download_tracks::mark_failed(pool, server_id, item_id, ino, &reason)).await?;
                return Ok(TrackDownloadOutcome::Failed(reason));
            }
        }

        let total_size = file_response.total_size;
        let mut stream = file_response.response.bytes_stream();
        let mut bytes_since_persist: u64 = 0;
        let mut stream_error: Option<(bool, String)> = None; // (retryable, reason)

        loop {
            if cancel() {
                save_progress(pool, server_id, item_id, ino, resume_offset, total_size.map(|n| n as i64)).await;
                return Ok(TrackDownloadOutcome::Canceled);
            }

            let next = match tokio::time::timeout(IDLE_READ_TIMEOUT, stream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    stream_error = Some((true, "connection stalled (no data received)".to_string()));
                    break;
                }
            };
            let Some(chunk) = next else { break }; // stream ended normally
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    stream_error = Some((true, err.to_string()));
                    break;
                }
            };

            if let Err(err) = file.write_all(&chunk).await {
                stream_error = Some((false, format!("disk write failed: {err}")));
                break;
            }
            resume_offset += chunk.len() as u64;
            bytes_since_persist += chunk.len() as u64;
            on_progress(resume_offset, total_size);

            if bytes_since_persist >= PROGRESS_PERSIST_INTERVAL_BYTES {
                save_progress(pool, server_id, item_id, ino, resume_offset, total_size.map(|n| n as i64)).await;
                bytes_since_persist = 0;
            }
        }
        let _ = file.flush().await;

        if let Some((retryable, reason)) = stream_error {
            save_progress(pool, server_id, item_id, ino, resume_offset, total_size.map(|n| n as i64)).await;
            if !retryable || attempt >= MAX_ATTEMPTS {
                retrying(|| abs_storage::repo::download_tracks::mark_failed(pool, server_id, item_id, ino, &reason)).await?;
                return Ok(TrackDownloadOutcome::Failed(reason));
            }
            backoff(attempt).await;
            continue;
        }

        // A connection that closes early must not look like success — only trust completion when
        // the byte count actually matches the size the server told us to expect.
        if !transfer_is_complete(resume_offset, total_size) {
            let expected = total_size.expect("transfer_is_complete only returns false when total_size is Some");
            let reason = format!("stream ended early: got {resume_offset} of {expected} bytes");
            save_progress(pool, server_id, item_id, ino, resume_offset, Some(expected as i64)).await;
            if attempt >= MAX_ATTEMPTS {
                retrying(|| abs_storage::repo::download_tracks::mark_failed(pool, server_id, item_id, ino, &reason)).await?;
                return Ok(TrackDownloadOutcome::Failed(reason));
            }
            backoff(attempt).await;
            continue;
        }

        // The counter says it's complete; the file itself has to agree before it's called that.
        // `sync_all` first, so the size read back is what's really on disk.
        let _ = file.sync_all().await;
        drop(file);
        let on_disk = tokio::fs::metadata(&path).await.map(|m| m.len()).ok();
        if on_disk != Some(resume_offset) {
            let reason = format!("the downloaded file is {} bytes, not the {resume_offset} received", on_disk.map_or("unreadable".to_string(), |n| n.to_string()));
            tracing::warn!(item_id, ino, %reason, "a finished download didn't check out; starting it over");
            resume_offset = 0;
            save_progress(pool, server_id, item_id, ino, 0, total_size.map(|n| n as i64)).await;
            if attempt >= MAX_ATTEMPTS {
                retrying(|| abs_storage::repo::download_tracks::mark_failed(pool, server_id, item_id, ino, &reason)).await?;
                return Ok(TrackDownloadOutcome::Failed(reason));
            }
            backoff(attempt).await;
            continue;
        }

        retrying(|| abs_storage::repo::download_tracks::mark_complete(pool, server_id, item_id, ino, resume_offset as i64)).await?;
        return Ok(TrackDownloadOutcome::Completed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ConnectionTarget;
    use std::cell::Cell;
    use std::rc::Rc;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn track(ino: &str, offset: f64, duration: f64) -> TrackRef {
        TrackRef { ino: ino.to_string(), track_index: 0, duration_seconds: duration, offset_seconds: offset, size_bytes: Some(1000) }
    }

    /// Like `track`, but with an unknown size — the "server didn't report it" case estimates
    /// must handle by refusing, not by guessing.
    fn unsized_track(ino: &str, offset: f64, duration: f64) -> TrackRef {
        TrackRef { ino: ino.to_string(), track_index: 0, duration_seconds: duration, offset_seconds: offset, size_bytes: None }
    }

    fn chapter(start: f64, end: f64) -> abs_api::ChapterRef {
        abs_api::ChapterRef { title: "Ch".to_string(), start_seconds: start, end_seconds: end }
    }

    #[test]
    fn tracks_needed_for_chapters_one_file_per_chapter() {
        let tracks = vec![track("a", 0.0, 100.0), track("b", 100.0, 100.0), track("c", 200.0, 100.0)];
        let chapters = vec![chapter(0.0, 100.0), chapter(100.0, 200.0), chapter(200.0, 300.0)];

        assert_eq!(tracks_needed_for_chapters(&tracks, &chapters, &[1]), BTreeSet::from(["b".to_string()]));
    }

    #[test]
    fn tracks_needed_for_chapters_one_file_many_chapters() {
        // A single track spanning every chapter — common for single-file audiobooks with
        // embedded chapter markers.
        let tracks = vec![track("only", 0.0, 300.0)];
        let chapters = vec![chapter(0.0, 100.0), chapter(100.0, 200.0), chapter(200.0, 300.0)];

        assert_eq!(tracks_needed_for_chapters(&tracks, &chapters, &[0]), BTreeSet::from(["only".to_string()]));
        assert_eq!(tracks_needed_for_chapters(&tracks, &chapters, &[2]), BTreeSet::from(["only".to_string()]));
    }

    #[test]
    fn tracks_needed_for_chapters_straddling_a_track_boundary() {
        let tracks = vec![track("a", 0.0, 150.0), track("b", 150.0, 150.0)];
        // This chapter starts in track "a" and ends in track "b".
        let chapters = vec![chapter(100.0, 200.0)];

        assert_eq!(tracks_needed_for_chapters(&tracks, &chapters, &[0]), BTreeSet::from(["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn tracks_needed_for_chapters_multiple_indices_union() {
        let tracks = vec![track("a", 0.0, 100.0), track("b", 100.0, 100.0), track("c", 200.0, 100.0)];
        let chapters = vec![chapter(0.0, 100.0), chapter(100.0, 200.0), chapter(200.0, 300.0)];

        assert_eq!(tracks_needed_for_chapters(&tracks, &chapters, &[0, 2]), BTreeSet::from(["a".to_string(), "c".to_string()]));
    }

    #[test]
    fn estimate_bytes_sums_the_distinct_tracks_a_scope_touches() {
        let tracks = vec![track("a", 0.0, 100.0), track("b", 100.0, 100.0), track("c", 200.0, 100.0)];
        let ranges: Vec<(f64, f64)> = vec![(0.0, 100.0), (100.0, 200.0), (200.0, 300.0)];

        // One chapter -> its one track; two chapters sharing nothing -> both; a shared file ->
        // counted once, not twice.
        assert_eq!(estimate_bytes_for_chapters(&tracks, &ranges, &[0]), Some(1000));
        assert_eq!(estimate_bytes_for_chapters(&tracks, &ranges, &[0, 2]), Some(2000));
        assert_eq!(estimate_bytes_for_chapters(&tracks, &ranges, &[0, 1]), Some(2000));
        assert_eq!(estimate_bytes_for_chapters(&[], &ranges, &[0]), None, "no cached tracks at all is 'can't know', not 'zero bytes'");
    }

    #[test]
    fn estimate_bytes_counts_a_shared_file_once_and_a_straddle_twice() {
        let one_shared = vec![track("only", 0.0, 300.0)];
        let ranges = vec![(0.0, 150.0), (150.0, 300.0)];
        assert_eq!(estimate_bytes_for_chapters(&one_shared, &ranges, &[0, 1]), Some(1000));

        let straddling = vec![track("a", 0.0, 150.0), track("b", 150.0, 150.0)];
        let straddle_chapters = vec![(100.0, 200.0)]; // starts in "a", ends in "b"
        assert_eq!(estimate_bytes_for_chapters(&straddling, &straddle_chapters, &[0]), Some(2000));
    }

    #[test]
    fn estimate_bytes_is_none_when_any_touched_track_has_an_unknown_size() {
        let tracks = vec![track("a", 0.0, 100.0), unsized_track("b", 100.0, 100.0), track("c", 200.0, 100.0)];
        let ranges: Vec<(f64, f64)> = vec![(0.0, 100.0), (100.0, 200.0), (200.0, 300.0)];

        assert_eq!(estimate_bytes_for_chapters(&tracks, &ranges, &[0]), Some(1000), "untouched unknowns don't spoil the estimate");
        assert_eq!(estimate_bytes_for_chapters(&tracks, &ranges, &[1]), None);
        assert_eq!(estimate_bytes_for_chapters(&tracks, &ranges, &[0, 1]), None);
    }

    #[test]
    fn chapter_offline_markers_reflects_which_tracks_are_complete() {
        let tracks = vec![track("a", 0.0, 100.0), track("b", 100.0, 100.0)];
        let chapters = vec![chapter(0.0, 100.0), chapter(100.0, 200.0)];
        let complete = BTreeSet::from(["a".to_string()]);

        assert_eq!(chapter_offline_markers(&tracks, &chapters, &complete), vec![true, false]);
    }

    #[test]
    fn chapter_offline_markers_requires_every_touched_track_complete() {
        let tracks = vec![track("a", 0.0, 150.0), track("b", 150.0, 150.0)];
        let chapters = vec![chapter(100.0, 200.0)]; // straddles both tracks
        let complete = BTreeSet::from(["a".to_string()]); // only one of the two

        assert_eq!(chapter_offline_markers(&tracks, &chapters, &complete), vec![false]);
    }

    #[tokio::test]
    async fn chapter_offline_markers_for_item_matches_the_pure_function() {
        // `pool_with_synced_tracks` gives every track the same offset (0.0) — fine for the tests
        // that use it, but this one needs two tracks at distinct offsets to tell them apart, so it
        // sets its own up directly via `abs_core::tracks::sync_item_tracks`.
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = abs_storage::repo::servers::add(&pool, "http://example.invalid").await.unwrap();
        abs_storage::repo::libraries::upsert(&pool, abs_storage::repo::libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 }).await.unwrap();
        abs_storage::repo::items::upsert(
            &pool,
            abs_storage::repo::items::UpsertItem { id: "item-1", server_id: &server_id, library_id: "lib-1", title: "Test Item", author: None, narrator: None, description: None, duration_seconds: 0.0, added_at: chrono::Utc::now(), series_name: None, genres: &[] },
        )
        .await
        .unwrap();
        crate::tracks::sync_item_tracks(
            &pool,
            &server_id,
            "item-1",
            &[
                crate::streaming::StreamTrack { ino: "a".to_string(), url: "u1".to_string(), duration_seconds: 50.0, offset_seconds: 0.0, size_bytes: None },
                crate::streaming::StreamTrack { ino: "b".to_string(), url: "u2".to_string(), duration_seconds: 50.0, offset_seconds: 50.0, size_bytes: None },
            ],
        )
        .await
        .unwrap();
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "a", "/p/a.mp3").await.unwrap();
        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 10).await.unwrap();

        let markers = chapter_offline_markers_for_item(&pool, &server_id, "item-1", &[(0.0, 50.0), (50.0, 100.0)]).await.unwrap();
        assert_eq!(markers, vec![true, false], "only the chapter fully within track 'a' should be marked downloaded");
    }

    async fn pool_with_synced_tracks(item_id: &str, inos: &[&str]) -> (SqlitePool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = abs_storage::repo::servers::add(&pool, "http://example.invalid").await.unwrap();
        abs_storage::repo::libraries::upsert(
            &pool,
            abs_storage::repo::libraries::UpsertLibrary { id: "lib-1", server_id: &server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        abs_storage::repo::items::upsert(
            &pool,
            abs_storage::repo::items::UpsertItem {
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
        let new_tracks: Vec<_> = inos.iter().map(|ino| abs_storage::repo::tracks::NewTrack { ino, duration_seconds: 100.0, offset_seconds: 0.0, size_bytes: None }).collect();
        abs_storage::repo::tracks::upsert_all(&pool, &server_id, item_id, &new_tracks).await.unwrap();
        (pool, server_id)
    }

    #[tokio::test]
    async fn item_offline_availability_progresses_from_none_to_partial_to_full() {
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["a", "b"]).await;
        assert_eq!(item_offline_availability(&pool, &server_id, "item-1").await.unwrap(), OfflineAvailability::None);

        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "a", "/p/a.mp3").await.unwrap();
        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 10).await.unwrap();
        assert_eq!(item_offline_availability(&pool, &server_id, "item-1").await.unwrap(), OfflineAvailability::Partial);

        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "b", "/p/b.mp3").await.unwrap();
        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "b", 10).await.unwrap();
        assert_eq!(item_offline_availability(&pool, &server_id, "item-1").await.unwrap(), OfflineAvailability::Full);
    }

    #[tokio::test]
    async fn complete_inos_for_item_includes_only_complete_tracks() {
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["a", "b"]).await;
        assert!(complete_inos_for_item(&pool, &server_id, "item-1").await.unwrap().is_empty());

        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "a", "/p/a.mp3").await.unwrap();
        abs_storage::repo::download_tracks::update_progress(&pool, &server_id, "item-1", "a", 5, Some(10)).await.unwrap();
        assert!(complete_inos_for_item(&pool, &server_id, "item-1").await.unwrap().is_empty(), "downloading-but-not-complete must not count");

        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 10).await.unwrap();
        assert_eq!(complete_inos_for_item(&pool, &server_id, "item-1").await.unwrap(), BTreeSet::from(["a".to_string()]));
    }

    #[tokio::test]
    async fn track_source_says_why_a_track_is_streamed() {
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["a"]).await;
        let source = || track_source(&pool, &server_id, "item-1", "a");
        assert_eq!(source().await, TrackSource::Streamed(StreamReason::NotDownloaded));

        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("a.mp3");
        tokio::fs::write(&file_path, b"hello").await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "a", file_path.to_str().unwrap()).await.unwrap();
        assert_eq!(source().await, TrackSource::Streamed(StreamReason::InProgress));

        abs_storage::repo::download_tracks::mark_failed(&pool, &server_id, "item-1", "a", "boom").await.unwrap();
        assert_eq!(source().await, TrackSource::Streamed(StreamReason::Failed));

        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 999).await.unwrap();
        assert_eq!(source().await, TrackSource::Streamed(StreamReason::SizeMismatch { expected: 999, actual: 5 }));

        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 5).await.unwrap();
        assert_eq!(source().await, TrackSource::Local(file_path.clone()));

        tokio::fs::remove_file(&file_path).await.unwrap();
        assert_eq!(source().await, TrackSource::Streamed(StreamReason::FileMissing));

        pool.close().await;
        assert!(matches!(source().await, TrackSource::Streamed(StreamReason::LookupFailed(_))), "a DB error is a reason, not a silent None");
    }

    #[tokio::test]
    async fn local_track_path_is_none_unless_complete_and_verified() {
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["a"]).await;
        assert!(local_track_path(&pool, &server_id, "item-1", "a").await.is_none(), "never downloaded");

        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("a.mp3");
        tokio::fs::write(&file_path, b"hello").await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "a", file_path.to_str().unwrap()).await.unwrap();
        assert!(local_track_path(&pool, &server_id, "item-1", "a").await.is_none(), "pending, not yet complete");

        abs_storage::repo::download_tracks::update_progress(&pool, &server_id, "item-1", "a", 5, Some(10)).await.unwrap();
        assert!(local_track_path(&pool, &server_id, "item-1", "a").await.is_none(), "downloading, not yet complete");

        // Mark complete claiming a size that doesn't match the real 5-byte file — a stale/corrupt
        // row must not be trusted.
        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 999).await.unwrap();
        assert!(local_track_path(&pool, &server_id, "item-1", "a").await.is_none(), "complete but size mismatch");

        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 5).await.unwrap();
        assert_eq!(local_track_path(&pool, &server_id, "item-1", "a").await, Some(file_path.clone()));

        tokio::fs::remove_file(&file_path).await.unwrap();
        assert!(local_track_path(&pool, &server_id, "item-1", "a").await.is_none(), "complete row whose file is gone must not be trusted");
    }

    #[tokio::test]
    async fn item_offline_availability_is_none_when_tracks_were_never_synced() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
        std::mem::forget(tmp);
        let server_id = abs_storage::repo::servers::add(&pool, "http://example.invalid").await.unwrap();

        assert_eq!(item_offline_availability(&pool, &server_id, "item-1").await.unwrap(), OfflineAvailability::None);
    }

    #[tokio::test]
    async fn clear_item_downloads_deletes_files_and_rows() {
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["a"]).await;
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("a.mp3");
        tokio::fs::write(&file_path, b"hello").await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "a", file_path.to_str().unwrap()).await.unwrap();
        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 5).await.unwrap();

        clear_item_downloads(&pool, &server_id, "item-1").await.unwrap();

        assert!(!file_path.exists());
        assert!(abs_storage::repo::download_tracks::list_for_item(&pool, &server_id, "item-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn clear_item_downloads_tolerates_an_already_missing_file() {
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["a"]).await;
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "a", "/does/not/exist.mp3").await.unwrap();
        abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", "a", 5).await.unwrap();

        // Must not error even though the file is already gone.
        clear_item_downloads(&pool, &server_id, "item-1").await.unwrap();
    }

    fn test_paths() -> (tempfile::TempDir, AppPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path().join("data"), tmp.path().join("cache"), tmp.path().join("state"));
        (tmp, paths)
    }

    fn no_cancel() -> impl Fn() -> bool {
        || false
    }

    #[tokio::test]
    async fn download_track_happy_path_writes_the_file_and_marks_complete() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "audio/mpeg").insert_header("Content-Length", "5").set_body_bytes(b"hello".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;

        let mut progress_calls = Vec::new();
        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |downloaded, total| progress_calls.push((downloaded, total)), &no_cancel())
            .await
            .unwrap();

        assert_eq!(outcome, TrackDownloadOutcome::Completed);
        assert!(!progress_calls.is_empty());

        let row = abs_storage::repo::download_tracks::get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Complete);
        assert_eq!(row.bytes_downloaded, 5);
        assert!(row.file_path.ends_with("ino-1.mp3"));
        assert_eq!(tokio::fs::read(&row.file_path).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn download_track_is_idempotent_once_complete() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "5").set_body_bytes(b"hello".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;

        download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();
        download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();

        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "a second call should be a cache hit, not a second HTTP request");
    }

    #[tokio::test]
    async fn download_track_a_404_fails_immediately_without_retrying() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/items/item-1/file/ino-1")).respond_with(ResponseTemplate::new(404)).mount(&mock_server).await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();

        assert!(matches!(outcome, TrackDownloadOutcome::Failed(_)));
        let requests = mock_server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "a 404 must not be retried");

        let row = abs_storage::repo::download_tracks::get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed);
    }

    #[tokio::test]
    async fn download_track_cancellation_leaves_a_resumable_partial_row() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "5").set_body_bytes(b"hello".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;

        // Cancel on the very first check, before any bytes are read at all.
        let canceled = Rc::new(Cell::new(true));
        let cancel_flag = canceled.clone();
        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &move || cancel_flag.get()).await.unwrap();

        assert_eq!(outcome, TrackDownloadOutcome::Canceled);
        assert!(abs_storage::repo::download_tracks::get(&pool, &server_id, "item-1", "ino-1").await.unwrap().is_none(), "canceling before any row exists must not fabricate one");
    }

    #[test]
    fn transfer_is_complete_requires_matching_byte_count_when_known() {
        assert!(!transfer_is_complete(5, Some(10)), "fewer bytes than promised must not look complete");
        assert!(transfer_is_complete(10, Some(10)));
        assert!(transfer_is_complete(5, None), "an unknown total size can't be verified, so it's trusted");
    }

    #[tokio::test]
    async fn download_track_resumes_from_a_previously_interrupted_session() {
        // Simulates the real scenario the ui-spec calls "resumable": the app was killed (or lost
        // connectivity) after a prior run wrote the first 5 bytes and recorded that progress:
        // re-invoking `download_track` must request only the remaining bytes via Range, not
        // restart from scratch.
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .and(header("Range", "bytes=5-"))
            .respond_with(ResponseTemplate::new(206).insert_header("Content-Range", "bytes 5-9/10").set_body_bytes(b"world".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;

        let file_path = paths.track_file_path(&server_id, "item-1", "ino-1", "mp3");
        tokio::fs::create_dir_all(file_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&file_path, b"hello").await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "ino-1", file_path.to_str().unwrap()).await.unwrap();
        abs_storage::repo::download_tracks::update_progress(&pool, &server_id, "item-1", "ino-1", 5, Some(10)).await.unwrap();

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();

        assert_eq!(outcome, TrackDownloadOutcome::Completed);
        assert_eq!(tokio::fs::read(&file_path).await.unwrap(), b"helloworld", "resumed bytes must be appended, not overwrite what was already there");
        let row = abs_storage::repo::download_tracks::get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.bytes_downloaded, 10);
        assert_eq!(row.status, DownloadStatus::Complete);
    }

    #[tokio::test]
    async fn download_track_falls_back_to_a_fresh_download_if_the_server_ignores_the_range_request() {
        // A server that doesn't support Range for this file replies 200 with the full body
        // instead of 206 — the resume attempt must detect this and restart from byte 0 rather
        // than append the full body onto the 5 bytes already on disk (which would duplicate data).
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .and(header("Range", "bytes=5-"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "10").set_body_bytes(b"full-bytes".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;

        let file_path = paths.track_file_path(&server_id, "item-1", "ino-1", "mp3");
        tokio::fs::create_dir_all(file_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&file_path, b"hello").await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "ino-1", file_path.to_str().unwrap()).await.unwrap();
        abs_storage::repo::download_tracks::update_progress(&pool, &server_id, "item-1", "ino-1", 5, Some(10)).await.unwrap();

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();

        assert_eq!(outcome, TrackDownloadOutcome::Completed);
        assert_eq!(tokio::fs::read(&file_path).await.unwrap(), b"full-bytes", "must restart fresh, not append the full body onto the old partial bytes");
    }

    /// A complete download of `body` at its usual path, as `download_track` would have left it.
    async fn complete_download(paths: &AppPaths, pool: &SqlitePool, server_id: &str, on_disk: &[u8], recorded_size: i64) -> PathBuf {
        let file_path = paths.track_file_path(server_id, "item-1", "ino-1", "mp3");
        tokio::fs::create_dir_all(file_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&file_path, on_disk).await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(pool, server_id, "item-1", "ino-1", file_path.to_str().unwrap()).await.unwrap();
        abs_storage::repo::download_tracks::mark_complete(pool, server_id, "item-1", "ino-1", recorded_size).await.unwrap();
        file_path
    }

    /// The phone's case: progress is saved every 256 KiB, so a download interrupted between saves
    /// leaves more bytes on disk than its checkpoint. Resuming must write from the checkpoint —
    /// it used to append after the end of the file, writing those bytes twice (a file 512 KiB too
    /// big on the phone, which playback then rightly refused).
    #[tokio::test]
    async fn download_track_resumes_from_the_saved_progress_not_the_end_of_the_file() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .and(header("Range", "bytes=5-"))
            .respond_with(ResponseTemplate::new(206).insert_header("Content-Range", "bytes 5-9/10").set_body_bytes(b"world".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;
        let file_path = paths.track_file_path(&server_id, "item-1", "ino-1", "mp3");
        tokio::fs::create_dir_all(file_path.parent().unwrap()).await.unwrap();
        // 5 bytes recorded, 3 more written after the last save.
        tokio::fs::write(&file_path, b"hellowor").await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "ino-1", file_path.to_str().unwrap()).await.unwrap();
        abs_storage::repo::download_tracks::update_progress(&pool, &server_id, "item-1", "ino-1", 5, Some(10)).await.unwrap();

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();

        assert_eq!(outcome, TrackDownloadOutcome::Completed);
        assert_eq!(tokio::fs::read(&file_path).await.unwrap(), b"helloworld", "the server's bytes, exactly — nothing written twice");
        assert!(matches!(track_source(&pool, &server_id, "item-1", "ino-1").await, TrackSource::Local(_)));
    }

    /// A partial file shorter than its saved progress was changed outside the download: its
    /// progress can't be trusted, so it downloads again from the first byte.
    #[tokio::test]
    async fn download_track_starts_over_when_the_file_is_shorter_than_its_saved_progress() {
        let mock_server = MockServer::start().await;
        // A resume request would get the tail; it must not be asked for.
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .and(header("Range", "bytes=5-"))
            .respond_with(ResponseTemplate::new(206).insert_header("Content-Range", "bytes 5-9/10").set_body_bytes(b"world".to_vec()))
            .with_priority(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "10").set_body_bytes(b"helloworld".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;
        let file_path = paths.track_file_path(&server_id, "item-1", "ino-1", "mp3");
        tokio::fs::create_dir_all(file_path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&file_path, b"hel").await.unwrap();
        abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", "ino-1", file_path.to_str().unwrap()).await.unwrap();
        abs_storage::repo::download_tracks::update_progress(&pool, &server_id, "item-1", "ino-1", 5, Some(10)).await.unwrap();

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();

        assert_eq!(outcome, TrackDownloadOutcome::Completed);
        assert_eq!(tokio::fs::read(&file_path).await.unwrap(), b"helloworld");
    }

    /// A finished download that is already damaged (the phone's chapter): playback notices, it
    /// stops counting as downloaded, and downloading it again gives a clean file.
    #[tokio::test]
    async fn a_damaged_download_stops_counting_as_downloaded_and_downloads_again_cleanly() {
        let mock_server = MockServer::start().await;
        // Any resume would corrupt it again: only a full download is right.
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .and(header_exists("Range"))
            .respond_with(ResponseTemplate::new(206).insert_header("Content-Range", "bytes 10-11/12").set_body_bytes(b"XX".to_vec()))
            .with_priority(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "10").set_body_bytes(b"helloworld".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;
        let file_path = complete_download(&paths, &pool, &server_id, b"hellowowworld", 10).await;

        let source = track_source(&pool, &server_id, "item-1", "ino-1").await;
        assert_eq!(source, TrackSource::Streamed(StreamReason::SizeMismatch { expected: 10, actual: 13 }));
        let row = abs_storage::repo::download_tracks::get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Failed, "a damaged download must stop counting as downloaded");
        assert!(row.error_reason.as_deref().is_some_and(|r| r.contains("damaged")), "got {:?}", row.error_reason);
        assert!(complete_inos_for_item(&pool, &server_id, "item-1").await.unwrap().is_empty());

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();
        assert_eq!(outcome, TrackDownloadOutcome::Completed);
        assert_eq!(tokio::fs::read(&file_path).await.unwrap(), b"helloworld");
        assert!(matches!(track_source(&pool, &server_id, "item-1", "ino-1").await, TrackSource::Local(_)));
    }

    /// Asking for a download re-checks what's already downloaded: an intact track is left alone,
    /// while one marked complete whose file is the wrong size, or gone, is fetched again (and no
    /// longer counts as downloaded meanwhile), along with anything failed or never downloaded.
    #[tokio::test]
    async fn tracks_to_fetch_rechecks_tracks_already_marked_downloaded() {
        let tmp = tempfile::tempdir().unwrap();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["intact", "truncated", "gone", "failed", "never"]).await;
        let complete = |ino: &'static str, on_disk: Option<&'static [u8]>| {
            let (pool, server_id) = (pool.clone(), server_id.clone());
            let file_path = tmp.path().join(format!("{ino}.mp3"));
            async move {
                if let Some(bytes) = on_disk {
                    tokio::fs::write(&file_path, bytes).await.unwrap();
                }
                abs_storage::repo::download_tracks::upsert_pending(&pool, &server_id, "item-1", ino, file_path.to_str().unwrap()).await.unwrap();
                abs_storage::repo::download_tracks::mark_complete(&pool, &server_id, "item-1", ino, 10).await.unwrap();
            }
        };
        complete("intact", Some(b"helloworld")).await;
        complete("truncated", Some(b"hello")).await;
        complete("gone", None).await;
        abs_storage::repo::download_tracks::mark_failed(&pool, &server_id, "item-1", "failed", "connection reset").await.unwrap();

        let all = ["intact", "truncated", "gone", "failed", "never"].map(String::from);
        let to_fetch = tracks_to_fetch(&pool, &server_id, "item-1", all).await;

        assert_eq!(to_fetch, ["truncated", "gone", "failed", "never"]);
        assert_eq!(
            complete_inos_for_item(&pool, &server_id, "item-1").await.unwrap(),
            BTreeSet::from(["intact".to_string()]),
            "the damaged ones no longer count as downloaded"
        );
    }

    /// Straight to `download_track`, without playback noticing first: a complete row whose file
    /// is the wrong size downloads again from the first byte, not from its recorded size.
    #[tokio::test]
    async fn download_track_redownloads_a_damaged_complete_file_from_the_start() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .and(header_exists("Range"))
            .respond_with(ResponseTemplate::new(416))
            .with_priority(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "10").set_body_bytes(b"helloworld".to_vec()))
            .mount(&mock_server)
            .await;

        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;
        let file_path = complete_download(&paths, &pool, &server_id, b"hellowowworld", 10).await;

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();
        assert_eq!(outcome, TrackDownloadOutcome::Completed);
        assert_eq!(tokio::fs::read(&file_path).await.unwrap(), b"helloworld");
    }

    /// Offline-first: checking a download needs only the device — no server exists at all here —
    /// and only proof of damage counts. A file that can't be checked right now plays from the
    /// device and keeps counting as downloaded.
    #[tokio::test]
    async fn checking_a_download_is_local_and_only_proof_of_damage_counts() {
        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;

        // A good download: local, no server anywhere.
        complete_download(&paths, &pool, &server_id, b"helloworld", 10).await;
        assert!(matches!(track_source(&pool, &server_id, "item-1", "ino-1").await, TrackSource::Local(_)));

        // A file that can't be checked (its "folder" is a file, so reading it fails with an error
        // other than not-found): played anyway, and still counted as downloaded.
        let blocker = paths.item_downloads_dir(&server_id, "item-1").join("not-a-dir");
        tokio::fs::write(&blocker, b"x").await.unwrap();
        let unreadable = blocker.join("ino-1.mp3");
        sqlx::query("UPDATE download_tracks SET file_path = ? WHERE ino = 'ino-1'").bind(unreadable.to_str().unwrap()).execute(&pool).await.unwrap();
        assert!(matches!(track_source(&pool, &server_id, "item-1", "ino-1").await, TrackSource::Local(_)), "a file that can't be checked plays from the device");
        assert_eq!(complete_inos_for_item(&pool, &server_id, "item-1").await.unwrap().len(), 1, "and keeps counting as downloaded");

        // Missing along with its whole folder (storage not mounted, say): streamed this time,
        // but not written off.
        let elsewhere = paths.item_downloads_dir(&server_id, "item-1").join("gone-folder").join("ino-1.mp3");
        sqlx::query("UPDATE download_tracks SET file_path = ? WHERE ino = 'ino-1'").bind(elsewhere.to_str().unwrap()).execute(&pool).await.unwrap();
        assert_eq!(track_source(&pool, &server_id, "item-1", "ino-1").await, TrackSource::Streamed(StreamReason::FileMissing));
        assert_eq!(complete_inos_for_item(&pool, &server_id, "item-1").await.unwrap().len(), 1, "a whole missing folder may come back; nothing is written off");

        // Missing from a folder that's there: that's proof, and it stops counting.
        let missing = paths.item_downloads_dir(&server_id, "item-1").join("ino-1-deleted.mp3");
        sqlx::query("UPDATE download_tracks SET file_path = ? WHERE ino = 'ino-1'").bind(missing.to_str().unwrap()).execute(&pool).await.unwrap();
        assert_eq!(track_source(&pool, &server_id, "item-1", "ino-1").await, TrackSource::Streamed(StreamReason::FileMissing));
        assert!(complete_inos_for_item(&pool, &server_id, "item-1").await.unwrap().is_empty());
    }

    /// Makes every progress save fail with a real database error, as the phone's did when a
    /// frozen main loop left the pool with no free connection. Only progress saves: the row's
    /// creation and its final "complete" go through.
    async fn fail_progress_saves(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TRIGGER fail_progress_saves BEFORE UPDATE ON download_tracks WHEN NEW.status = 'downloading'
             BEGIN SELECT RAISE(ABORT, 'simulated: the database is too busy'); END",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// 600 KiB of distinct bytes: more than two progress-save intervals.
    fn large_body() -> Vec<u8> {
        (0..600 * 1024).map(|i: u32| (i % 251) as u8).collect()
    }

    /// The phone's failure: progress saves failing (the pool too busy to answer in time) used to
    /// end the download. Saving progress is bookkeeping; the transfer carries on and completes.
    #[tokio::test]
    async fn a_download_keeps_going_when_its_progress_saves_fail() {
        let body = large_body();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", body.len().to_string().as_str()).set_body_bytes(body.clone()))
            .mount(&mock_server)
            .await;
        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;
        fail_progress_saves(&pool).await;

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await;

        assert_eq!(outcome.ok(), Some(TrackDownloadOutcome::Completed), "failed progress saves must not end the download");
        let row = abs_storage::repo::download_tracks::get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Complete);
        assert_eq!(tokio::fs::read(&row.file_path).await.unwrap(), body, "the server's bytes, exactly");
    }

    /// Cancelling while progress can't be saved still cancels; the row then holds an older
    /// checkpoint than the file, and the next download still comes out exact.
    #[tokio::test]
    async fn cancelling_while_progress_saves_fail_still_resumes_to_an_exact_file() {
        let body = large_body();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", body.len().to_string().as_str()).set_body_bytes(body.clone()))
            .mount(&mock_server)
            .await;
        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;
        fail_progress_saves(&pool).await;

        // Cancel once some bytes are on disk.
        let checks = std::rc::Rc::new(std::cell::Cell::new(0_u32));
        let cancel = {
            let checks = checks.clone();
            move || {
                checks.set(checks.get() + 1);
                checks.get() > 4
            }
        };
        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &cancel).await;
        assert_eq!(outcome.ok(), Some(TrackDownloadOutcome::Canceled));

        sqlx::query("DROP TRIGGER fail_progress_saves").execute(&pool).await.unwrap();
        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await.unwrap();
        assert_eq!(outcome, TrackDownloadOutcome::Completed);
        let row = abs_storage::repo::download_tracks::get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(tokio::fs::read(&row.file_path).await.unwrap(), body);
    }

    /// A finished download whose "complete" can't be recorded on the first try (a short
    /// database stall) is retried rather than thrown away.
    #[tokio::test]
    async fn recording_a_finished_download_rides_out_a_short_stall() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "10").set_body_bytes(b"helloworld".to_vec()))
            .mount(&mock_server)
            .await;
        let (_tmp, paths) = test_paths();
        let (pool, server_id) = pool_with_synced_tracks("item-1", &["ino-1"]).await;
        // Rejects the first "complete" only (RAISE(FAIL) keeps the counter's update).
        sqlx::query("CREATE TABLE completions_seen (n INTEGER NOT NULL)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO completions_seen VALUES (0)").execute(&pool).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_first_completion BEFORE UPDATE ON download_tracks
             WHEN NEW.status = 'complete' AND (SELECT n FROM completions_seen) = 0
             BEGIN UPDATE completions_seen SET n = 1; SELECT RAISE(FAIL, 'simulated: the database is too busy'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let outcome = download_track(&paths, &pool, &ConnectionTarget::direct(&mock_server.uri()), "token", &server_id, "item-1", "ino-1", |_, _| {}, &no_cancel()).await;

        assert_eq!(outcome.ok(), Some(TrackDownloadOutcome::Completed));
        let seen: i64 = sqlx::query_scalar("SELECT n FROM completions_seen").fetch_one(&pool).await.unwrap();
        assert_eq!(seen, 1, "the first try really was rejected");
        let row = abs_storage::repo::download_tracks::get(&pool, &server_id, "item-1", "ino-1").await.unwrap().unwrap();
        assert_eq!(row.status, DownloadStatus::Complete);
    }
}
