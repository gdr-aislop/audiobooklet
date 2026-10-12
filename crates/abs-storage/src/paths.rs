//! Resolves the on-disk locations this app uses, following the XDG Base Directory layout
//! documented in the architecture plan: durable user data under `$XDG_DATA_HOME`, evictable
//! cache under `$XDG_CACHE_HOME`, and operational state (logs, crash dumps — not user data, not
//! safe to evict like a cache) under `$XDG_STATE_HOME`. Nothing outside this module computes one
//! of these paths directly.

use std::path::{Path, PathBuf};

use directories::BaseDirs;

/// The app's reverse-DNS identity — also the GTK application id and, on Flatpak, the sandbox's
/// per-app data directory name, so this is deliberately used verbatim as the directory name
/// below rather than through `directories::ProjectDirs`. `ProjectDirs::from(qualifier,
/// organization, application)` looks like the natural fit, but on Linux it ignores `qualifier`
/// and `organization` entirely and just lowercases `application` — confirmed by actually running
/// it, not assumed — which would have put this app's data under `~/.local/share/audiobookshelf/`
/// instead of the reverse-DNS-named directory GNOME apps (and Flatpak) actually use.
///
/// Matches the Flatpak manifest's `app-id` (`flatpak/io.github.gdr_aislop.audiobooklet.json`) exactly.
/// The GitHub owner segment uses an underscore rather than its real hyphen (`gdr-aislop`) because
/// `flatpak-builder` rejects a hyphen in any but the last dotted segment ("Only last name segment
/// can contain -"), even though a mid-element hyphen is otherwise valid in a `GApplication` id
/// (`g_application_id_is_valid` only forbids a leading hyphen per element). Flatpak's stricter rule
/// wins here so this id can stay one string across AppStream metainfo, the Flatpak manifest, and
/// this data directory, instead of forking it per packaging format.
pub const APP_ID: &str = "io.github.gdr_aislop.audiobooklet";

/// The app ID up to 0.9.5, when the app was called abs-app. Its directories are moved to
/// [`APP_ID`]'s on the first start after the rename — see [`AppPaths::migrate_legacy_dirs`].
pub const LEGACY_APP_ID: &str = "io.github.gdr_aislop.abs-app";

/// File-name prefix of the rolling log files in [`AppPaths::logs_dir`]. The `app` crate's
/// logger writes with it and [`AppPaths::latest_log_file`] looks for it, so the two can't drift.
pub const LOG_FILE_PREFIX: &str = "audiobooklet";

/// What [`AppPaths::migrate_legacy_dirs`] did with one of the app's directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyDirMove {
    /// The old directory became the new one.
    Moved { from: PathBuf, to: PathBuf },
    /// Both exist, so nothing was moved: the new one is in use, the old one is left for the user.
    BothExist { legacy: PathBuf, current: PathBuf },
    /// The move failed; the app starts with an empty new directory.
    Failed { from: PathBuf, to: PathBuf, error: String },
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    data_dir: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
}

impl AppPaths {
    /// Resolve paths from the real XDG environment. Returns `None` if no home directory can be
    /// determined at all (extremely unusual — `directories` falls back sensibly otherwise).
    pub fn resolve() -> Option<Self> {
        let dirs = BaseDirs::new()?;
        Some(Self {
            data_dir: dirs.data_dir().join(APP_ID),
            cache_dir: dirs.cache_dir().join(APP_ID),
            // `state_dir()` is `None` on platforms with no `$XDG_STATE_HOME` equivalent — falls
            // back to `data_dir` (durable-adjacent) rather than `cache_dir` (evictable), matching
            // why logs/crash dumps don't belong in the cache dir in the first place.
            state_dir: dirs.state_dir().unwrap_or_else(|| dirs.data_dir()).join(APP_ID),
        })
    }

    /// Build an instance rooted at arbitrary directories — used by tests (and could be used for
    /// a future multi-profile mode) instead of touching the real user environment.
    pub fn rooted_at(data_dir: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            cache_dir: cache_dir.into(),
            state_dir: state_dir.into(),
        }
    }

    /// `(old, new)` for each of the data, cache and state directories: where the app kept it
    /// under [`LEGACY_APP_ID`], next to where it keeps it now. Duplicates (a state directory that
    /// falls back to the data directory) are listed once.
    pub fn legacy_pairs(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
        for current in [&self.data_dir, &self.cache_dir, &self.state_dir] {
            let Some(parent) = current.parent() else { continue };
            let pair = (parent.join(LEGACY_APP_ID), current.clone());
            if pair.0 != pair.1 && !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
        pairs
    }

    /// Moves the directories the app used before its rename (see [`LEGACY_APP_ID`]) to the
    /// current ones, so the login, settings and downloads carry over. Only an old directory whose
    /// new counterpart doesn't exist yet is moved, so this is a no-op after the first start — and
    /// must run before anything creates the new directories. The database still holds absolute
    /// paths into the old directories; `repo::relocate::rebase_paths` rewrites those.
    pub fn migrate_legacy_dirs(&self) -> Vec<LegacyDirMove> {
        let mut outcomes = Vec::new();
        for (from, to) in self.legacy_pairs() {
            if !from.is_dir() {
                continue;
            }
            if to.exists() {
                outcomes.push(LegacyDirMove::BothExist { legacy: from, current: to });
                continue;
            }
            if let Some(parent) = to.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            outcomes.push(match std::fs::rename(&from, &to) {
                Ok(()) => LegacyDirMove::Moved { from, to },
                Err(err) => LegacyDirMove::Failed { from, to, error: err.to_string() },
            });
        }
        outcomes
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Rotating application logs — see `crash_reporting::init_logging` in the `app` crate.
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }

    /// The newest log file in [`logs_dir`](Self::logs_dir), or `None` if there isn't one (nothing
    /// logged yet, or the directory doesn't exist). Chosen by file name, not modification time:
    /// the rolling appender suffixes each file with its date (`audiobooklet.2026-10-09`), which sorts
    /// chronologically and can't be skewed by a file being touched or restored from a backup.
    pub fn latest_log_file(&self) -> std::io::Result<Option<PathBuf>> {
        let entries = match std::fs::read_dir(self.logs_dir()) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let mut newest: Option<(std::ffi::OsString, PathBuf)> = None;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let is_log = name.to_str().is_some_and(|n| {
                n == LOG_FILE_PREFIX || n.strip_prefix(LOG_FILE_PREFIX).is_some_and(|rest| rest.starts_with('.'))
            });
            if is_log && newest.as_ref().is_none_or(|(best, _)| name > *best) {
                newest = Some((name, entry.path()));
            }
        }
        Ok(newest.map(|(_, path)| path))
    }

    /// Local Breakpad-format `.dmp` files written on a native (signal-level) crash — see
    /// `crash_reporting::attach_crash_handler` in the `app` crate.
    pub fn crash_dumps_dir(&self) -> PathBuf {
        self.state_dir.join("crashes")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("db.sqlite3")
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.data_dir.join("downloads")
    }

    /// Directory holding downloaded chapter files for one item on one server.
    pub fn item_downloads_dir(&self, server_id: &str, item_id: &str) -> PathBuf {
        self.downloads_dir().join(server_id).join(item_id)
    }

    /// Path a given track (audio file, keyed by the server's own `ino`) of an item would be
    /// downloaded to. `extension` must be the real extension for the track's content-type (e.g.
    /// `"mp3"`, `"m4b"`, `"m4a"`) — Audiobookshelf items aren't always mp3, so callers must derive
    /// it from the actual response the same way `cover_cache_path` already requires for covers.
    pub fn track_file_path(&self, server_id: &str, item_id: &str, ino: &str, extension: &str) -> PathBuf {
        self.item_downloads_dir(server_id, item_id).join(format!("{ino}.{extension}"))
    }

    pub fn covers_dir(&self) -> PathBuf {
        self.cache_dir.join("covers")
    }

    /// `extension` is the real extension for the cover bytes being cached (e.g. `"webp"`,
    /// `"png"`, `"jpg"`) — Audiobookshelf serves covers in whatever format the source file is in,
    /// not always JPEG, so callers must pass the actual content-type-derived extension rather
    /// than assuming one.
    pub fn cover_cache_path(&self, server_id: &str, item_id: &str, extension: &str) -> PathBuf {
        self.covers_dir()
            .join(server_id)
            .join(format!("{item_id}.{extension}"))
    }

    /// The cover as uploaded to the server (the full-size original, fetched only when someone
    /// opens the cover full-screen), next to the resized copy [`Self::cover_cache_path`] holds.
    pub fn original_cover_path(&self, server_id: &str, item_id: &str, extension: &str) -> PathBuf {
        self.covers_dir().join(server_id).join(format!("{item_id}.original.{extension}"))
    }

    /// Where the covers cache remembers that the server had no cover for an item (an empty file
    /// whose modification time says when it last answered 404) — see `abs_core::covers`.
    pub fn missing_cover_marker_path(&self, server_id: &str, item_id: &str) -> PathBuf {
        self.covers_dir().join(server_id).join(format!("{item_id}.no-cover"))
    }

    /// Delete every cached cover file of one item — the resized cover, the full-size original and
    /// the "server has no cover" marker, whatever their extensions. Used when sync drops a book
    /// the server no longer has. Best-effort: a missing directory or file is fine, other errors
    /// are logged.
    pub async fn purge_item_covers(&self, server_id: &str, item_id: &str) {
        let dir = self.covers_dir().join(server_id);
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else { return };
        let prefix = format!("{item_id}.");
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                if let Err(err) = tokio::fs::remove_file(entry.path()).await {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(%err, item_id, "couldn't delete a cached cover");
                    }
                }
            }
        }
    }

    /// Every directory this `AppPaths` might write into — the single list `ensure_dirs` (async,
    /// called once `setup()` has a Tokio runtime) and `main.rs`'s early synchronous creation of
    /// just the logging/crash-dump dirs (needed before a runtime exists) both draw from, so the
    /// two can't drift apart.
    fn all_dirs(&self) -> [PathBuf; 6] {
        [
            self.data_dir.clone(),
            self.downloads_dir(),
            self.cache_dir.clone(),
            self.covers_dir(),
            self.logs_dir(),
            self.crash_dumps_dir(),
        ]
    }

    /// Create every directory this `AppPaths` might write into. Idempotent.
    pub async fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in self.all_dirs() {
            tokio::fs::create_dir_all(dir).await?;
        }
        Ok(())
    }

    /// Synchronous, `logs_dir`/`crash_dumps_dir`-only subset of `ensure_dirs` — for `main.rs` to
    /// call before a Tokio runtime exists, since logging and crash-dump capture must be wired up
    /// before the rest of async setup runs (so they can catch failures during that setup too).
    /// `ensure_dirs` still creates these same two directories again later (idempotent) alongside
    /// everything else.
    pub fn ensure_early_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.logs_dir())?;
        std::fs::create_dir_all(self.crash_dumps_dir())?;
        Ok(())
    }

    /// Delete everything on disk scoped to one server — its cover-cache subtree
    /// (`covers/<server_id>/`) and its downloads subtree (`downloads/<server_id>/`). Used when a
    /// re-login switches to a different server and the old one's cached files would otherwise sit
    /// orphaned forever. Missing directories are fine (nothing was ever downloaded); any other
    /// error surfaces to the caller, which decides it's non-fatal — stale files are wasteful, not
    /// harmful.
    pub async fn purge_server_data(&self, server_id: &str) -> std::io::Result<()> {
        let mut first_error = None;
        for dir in [self.downloads_dir().join(server_id), self.covers_dir().join(server_id)] {
            if let Err(err) = tokio::fs::remove_dir_all(&dir).await {
                if err.kind() != std::io::ErrorKind::NotFound && first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

/// Bytes available to this (unprivileged) process on the filesystem holding `path` — what the
/// download sheet's size estimates are compared against (ui-spec: an option whose estimate
/// exceeds free space is refused up front instead of failing partway through). `None` when it
/// can't be determined (the path doesn't exist yet, an exotic filesystem) — callers must treat
/// that as "don't restrict", never as "zero": an unknown free space must not block downloads.
pub fn free_space_bytes(path: &Path) -> Option<u64> {
    let stats = rustix::fs::statvfs(path).ok()?;
    Some(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths() -> (tempfile::TempDir, AppPaths) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let paths = AppPaths::rooted_at(tmp.path().join("data"), tmp.path().join("cache"), tmp.path().join("state"));
        (tmp, paths)
    }

    #[test]
    fn free_space_is_some_positive_for_an_existing_dir_and_none_for_a_missing_one() {
        let tmp = tempfile::tempdir().unwrap();
        let free = free_space_bytes(tmp.path());
        assert!(free.is_some_and(|bytes| bytes > 0), "a real directory always has some free space");

        assert_eq!(free_space_bytes(&tmp.path().join("does-not-exist")), None, "a missing path can't be probed — treated as unknown, not zero");
    }

    #[test]
    fn db_path_lives_under_data_dir() {
        let (_tmp, paths) = test_paths();
        assert_eq!(paths.db_path(), paths.data_dir().join("db.sqlite3"));
    }

    #[test]
    fn item_downloads_dir_is_scoped_by_server_and_item() {
        let (_tmp, paths) = test_paths();
        let a = paths.item_downloads_dir("server-a", "item-1");
        let b = paths.item_downloads_dir("server-b", "item-1");
        let c = paths.item_downloads_dir("server-a", "item-2");
        assert_ne!(a, b, "different servers must not share a download directory");
        assert_ne!(a, c, "different items must not share a download directory");
        assert!(a.starts_with(paths.downloads_dir()));
    }

    #[test]
    fn track_file_path_is_named_after_the_ino_with_the_given_extension() {
        let (_tmp, paths) = test_paths();
        let mp3 = paths.track_file_path("s", "i", "12345", "mp3");
        let m4b = paths.track_file_path("s", "i", "67890", "m4b");
        assert_eq!(mp3.file_name().unwrap(), "12345.mp3");
        assert_eq!(m4b.file_name().unwrap(), "67890.m4b");
        assert!(mp3.starts_with(paths.item_downloads_dir("s", "i")));
    }

    #[test]
    fn cover_cache_path_lives_under_cache_dir_not_data_dir() {
        let (_tmp, paths) = test_paths();
        let cover = paths.cover_cache_path("server-a", "item-1", "jpg");
        assert!(cover.starts_with(paths.cache_dir()));
        assert!(!cover.starts_with(paths.data_dir()));
    }

    #[test]
    fn logs_dir_and_crash_dumps_dir_live_under_state_dir_not_cache_or_data() {
        let (_tmp, paths) = test_paths();
        for dir in [paths.logs_dir(), paths.crash_dumps_dir()] {
            assert!(dir.starts_with(paths.state_dir()));
            assert!(!dir.starts_with(paths.cache_dir()));
            assert!(!dir.starts_with(paths.data_dir()));
        }
        assert_ne!(paths.logs_dir(), paths.crash_dumps_dir());
    }

    #[test]
    fn latest_log_file_is_none_without_a_logs_dir_or_logs() {
        let (_tmp, paths) = test_paths();
        assert_eq!(paths.latest_log_file().unwrap(), None, "no directory yet");
        std::fs::create_dir_all(paths.logs_dir()).unwrap();
        assert_eq!(paths.latest_log_file().unwrap(), None, "empty directory");
    }

    #[test]
    fn latest_log_file_picks_the_newest_date_even_when_mtimes_disagree() {
        let (_tmp, paths) = test_paths();
        let dir = paths.logs_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // Written newest-date-first, so the newest *date* has the oldest mtime.
        for day in ["2026-10-09", "2026-10-07", "2026-10-08"] {
            std::fs::write(dir.join(format!("audiobooklet.{day}")), day).unwrap();
        }
        assert_eq!(paths.latest_log_file().unwrap(), Some(dir.join("audiobooklet.2026-10-09")));
    }

    #[test]
    fn latest_log_file_ignores_other_files_and_directories() {
        let (_tmp, paths) = test_paths();
        let dir = paths.logs_dir();
        std::fs::create_dir_all(dir.join("audiobooklet.2099-01-01")).unwrap(); // a directory
        std::fs::write(dir.join("other.log"), "").unwrap();
        std::fs::write(dir.join("audiobooklet-old.txt"), "").unwrap();
        std::fs::create_dir_all(paths.crash_dumps_dir()).unwrap();
        std::fs::write(paths.crash_dumps_dir().join("audiobooklet.2099-01-01"), "").unwrap();
        assert_eq!(paths.latest_log_file().unwrap(), None);

        std::fs::write(dir.join("audiobooklet"), "undated").unwrap();
        assert_eq!(paths.latest_log_file().unwrap(), Some(dir.join("audiobooklet")), "the undated name is still a log");
    }

    #[test]
    fn cover_cache_path_uses_the_given_extension() {
        let (_tmp, paths) = test_paths();
        let cover = paths.cover_cache_path("server-a", "item-1", "webp");
        assert_eq!(cover.extension().unwrap(), "webp");
    }

    #[tokio::test]
    async fn ensure_dirs_creates_every_directory_it_promises() {
        let (_tmp, paths) = test_paths();
        paths.ensure_dirs().await.expect("ensure_dirs succeeds");

        assert!(paths.data_dir().is_dir());
        assert!(paths.downloads_dir().is_dir());
        assert!(paths.cache_dir().is_dir());
        assert!(paths.covers_dir().is_dir());
        assert!(paths.logs_dir().is_dir());
        assert!(paths.crash_dumps_dir().is_dir());
    }

    #[test]
    fn ensure_early_dirs_creates_only_logs_and_crash_dumps() {
        let (_tmp, paths) = test_paths();
        paths.ensure_early_dirs().expect("ensure_early_dirs succeeds");

        assert!(paths.logs_dir().is_dir());
        assert!(paths.crash_dumps_dir().is_dir());
        assert!(!paths.data_dir().exists(), "ensure_early_dirs must not create the other, async-created dirs");
    }
    #[tokio::test]
    async fn ensure_dirs_is_idempotent() {
        let (_tmp, paths) = test_paths();
        paths.ensure_dirs().await.expect("first call succeeds");

        paths
            .ensure_dirs()
            .await
            .expect("second call on already-existing dirs also succeeds");
    }

    #[tokio::test]
    async fn purge_server_data_removes_only_that_servers_subtrees() {
        let (_tmp, paths) = test_paths();
        paths.ensure_dirs().await.unwrap();

        let old_cover = paths.cover_cache_path("old-server", "item-1", "jpg");
        tokio::fs::create_dir_all(old_cover.parent().unwrap()).await.unwrap();
        tokio::fs::write(&old_cover, b"bytes").await.unwrap();
        let old_download = paths.item_downloads_dir("old-server", "item-1");
        tokio::fs::create_dir_all(&old_download).await.unwrap();
        tokio::fs::write(old_download.join("001-ch.mp3"), b"bytes").await.unwrap();
        let kept_cover = paths.cover_cache_path("kept-server", "item-1", "jpg");
        tokio::fs::create_dir_all(kept_cover.parent().unwrap()).await.unwrap();
        tokio::fs::write(&kept_cover, b"bytes").await.unwrap();

        paths.purge_server_data("old-server").await.expect("purge succeeds");

        assert!(!old_cover.exists(), "the old server's covers must go");
        assert!(!old_download.exists(), "the old server's downloads must go");
        assert!(kept_cover.exists(), "other servers' files must be untouched");
    }

    #[tokio::test]
    async fn purge_server_data_tolerates_a_server_that_never_downloaded_anything() {
        let (_tmp, paths) = test_paths();
        paths.ensure_dirs().await.unwrap();

        paths.purge_server_data("no-such-server").await.expect("missing dirs are a no-op, not an error");
    }

    #[test]
    fn resolve_produces_distinct_data_cache_and_state_dirs() {
        // This touches the real environment (HOME/XDG_*), so just check internal consistency
        // rather than asserting a specific path — the sandbox running this test may not have a
        // conventional home directory.
        if let Some(paths) = AppPaths::resolve() {
            assert_ne!(paths.data_dir(), paths.cache_dir());
            assert_ne!(paths.data_dir(), paths.state_dir());
            assert_ne!(paths.cache_dir(), paths.state_dir());
        }
    }

    #[test]
    fn migrating_moves_the_legacy_directories_once() {
        let base = tempfile::tempdir().unwrap();
        let (data, cache, state) = (base.path().join("share"), base.path().join("cache"), base.path().join("state"));
        let paths = AppPaths::rooted_at(data.join(APP_ID), cache.join(APP_ID), state.join(APP_ID));
        std::fs::create_dir_all(data.join(LEGACY_APP_ID).join("downloads")).unwrap();
        std::fs::write(data.join(LEGACY_APP_ID).join("abs.db"), "db").unwrap();
        std::fs::create_dir_all(state.join(LEGACY_APP_ID).join("logs")).unwrap();
        // No legacy cache directory at all: nothing to report for it.

        let outcomes = paths.migrate_legacy_dirs();
        assert_eq!(
            outcomes,
            vec![
                LegacyDirMove::Moved { from: data.join(LEGACY_APP_ID), to: data.join(APP_ID) },
                LegacyDirMove::Moved { from: state.join(LEGACY_APP_ID), to: state.join(APP_ID) },
            ]
        );
        assert_eq!(std::fs::read_to_string(data.join(APP_ID).join("abs.db")).unwrap(), "db", "contents move along");
        assert!(data.join(APP_ID).join("downloads").is_dir());
        assert!(!data.join(LEGACY_APP_ID).exists(), "the old directory is gone, not copied");

        assert!(paths.migrate_legacy_dirs().is_empty(), "a second start has nothing left to move");
    }

    #[test]
    fn migrating_leaves_both_alone_when_the_new_directory_already_exists() {
        let base = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(base.path().join("a").join(APP_ID), base.path().join("b").join(APP_ID), base.path().join("c").join(APP_ID));
        std::fs::create_dir_all(base.path().join("a").join(LEGACY_APP_ID)).unwrap();
        std::fs::create_dir_all(base.path().join("a").join(APP_ID)).unwrap();
        std::fs::write(base.path().join("a").join(APP_ID).join("new"), "").unwrap();

        assert_eq!(
            paths.migrate_legacy_dirs(),
            vec![LegacyDirMove::BothExist { legacy: base.path().join("a").join(LEGACY_APP_ID), current: base.path().join("a").join(APP_ID) }]
        );
        assert!(base.path().join("a").join(APP_ID).join("new").exists(), "the directory in use is untouched");
        assert!(base.path().join("a").join(LEGACY_APP_ID).exists(), "the old one is left for the user");
    }

    #[test]
    fn legacy_pairs_list_a_shared_directory_once() {
        let paths = AppPaths::rooted_at("/x/share/new", "/x/cache/new", "/x/share/new");
        assert_eq!(
            paths.legacy_pairs(),
            vec![
                (PathBuf::from("/x/share").join(LEGACY_APP_ID), PathBuf::from("/x/share/new")),
                (PathBuf::from("/x/cache").join(LEGACY_APP_ID), PathBuf::from("/x/cache/new")),
            ]
        );
    }

    #[test]
    fn resolve_uses_the_literal_app_id_as_the_directory_name() {
        // Regression test: `ProjectDirs::from(qualifier, org, application)` ignores qualifier
        // and organization on Linux and lowercases `application`, which would silently put data
        // under `~/.local/share/audiobookshelf/` instead of the reverse-DNS-named directory this
        // app (and Flatpak) actually expects — confirmed by running it, not assumed. `resolve()`
        // must build the path from `BaseDirs` + the literal `APP_ID`, not `ProjectDirs`.
        if let Some(paths) = AppPaths::resolve() {
            assert_eq!(paths.data_dir().file_name().unwrap(), APP_ID);
            assert_eq!(paths.cache_dir().file_name().unwrap(), APP_ID);
            assert_eq!(paths.state_dir().file_name().unwrap(), APP_ID);
        }
    }
}
