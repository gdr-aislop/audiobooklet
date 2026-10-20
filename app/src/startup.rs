//! Opening the app's local data at startup, and what to tell the user when that fails.
//!
//! Everything here used to be a chain of `.expect()`s run before any window existed, so a
//! failure was a silent exit with one line in the log: an older copy of the app meeting a
//! database a newer copy had migrated (an installed package next to a newer AppImage, sharing
//! one data folder), a full disk, a read-only or foreign-owned data folder, a database locked
//! by another copy, or a damaged database file. [`open`] returns a [`StartupProblem`] instead,
//! and `screens::startup_problem` shows it with a way out.
//!
//! "Damaged" is reserved for a database SQLite itself can't read (`SQLITE_CORRUPT`,
//! `SQLITE_NOTADB`), confirmed by a separate read-only integrity check, because it is the one
//! problem offered "Start over". A database that is merely newer than this version, or whose
//! upgrade to this version failed, is never called damaged: its data is fine, and starting
//! over would throw it away.

use std::path::{Path, PathBuf};

use abs_storage::StorageError;
use sqlx::migrate::MigrateError;

use crate::application::AppState;
use crate::i18n::{tr, tr_args};

/// What went wrong, grouped by what the user can do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProblemKind {
    /// The database was last opened (and migrated) by a newer version of the app.
    NeedsNewerApp,
    /// Bringing the database up to this version failed — not for lack of space or access.
    UpgradeFailed,
    OutOfSpace,
    /// The data folder or database file can't be written: permissions, a read-only filesystem.
    NoAccess,
    /// Another process holds the database (another copy of the app, say).
    InUse,
    /// SQLite can't read the database file at all — confirmed, see the module doc.
    Damaged,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct StartupProblem {
    pub kind: ProblemKind,
    /// The raw error, for the "Details" disclosure and the log.
    pub details: String,
    /// The file or folder involved, where one is.
    pub path: Option<PathBuf>,
}

/// How this copy of the app was installed, which decides how it is updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallKind {
    AppImage,
    Flatpak,
    /// A distribution package (the .deb), updated with the system's software updates.
    Package,
}

impl InstallKind {
    pub(crate) fn detect() -> Self {
        Self::from_env(std::env::var_os("APPIMAGE").is_some(), std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists())
    }

    fn from_env(appimage: bool, flatpak: bool) -> Self {
        if appimage {
            InstallKind::AppImage
        } else if flatpak {
            InstallKind::Flatpak
        } else {
            InstallKind::Package
        }
    }
}

/// Where the latest release can be downloaded — the same link for every way of installing.
pub(crate) const LATEST_RELEASE_URL: &str = "https://github.com/gdr-aislop/audiobooklet/releases/latest";

// SQLite's primary result codes (an extended code's low byte).
const SQLITE_PERM: i64 = 3;
const SQLITE_BUSY: i64 = 5;
const SQLITE_LOCKED: i64 = 6;
const SQLITE_READONLY: i64 = 8;
const SQLITE_CORRUPT: i64 = 11;
const SQLITE_FULL: i64 = 13;
const SQLITE_CANTOPEN: i64 = 14;
const SQLITE_NOTADB: i64 = 26;

impl StartupProblem {
    pub(crate) fn new(kind: ProblemKind, details: impl Into<String>, path: Option<PathBuf>) -> Self {
        Self { kind, details: details.into(), path }
    }

    /// A failure opening, migrating or reading the database at `db_path`.
    pub(crate) fn from_storage(err: &StorageError, db_path: &Path) -> Self {
        let kind = match err {
            // A version this binary doesn't know was applied: a newer copy migrated the database.
            StorageError::Migration(MigrateError::VersionMissing(_)) => ProblemKind::NeedsNewerApp,
            // Every other migration failure is an upgrade that didn't go through. Space, access
            // and locks still say so; nothing raised while migrating is ever "damaged".
            StorageError::Migration(MigrateError::Execute(inner) | MigrateError::ExecuteMigration(inner, _)) => match sqlx_kind(inner) {
                kind @ (ProblemKind::OutOfSpace | ProblemKind::NoAccess | ProblemKind::InUse) => kind,
                _ => ProblemKind::UpgradeFailed,
            },
            StorageError::Migration(_) => ProblemKind::UpgradeFailed,
            StorageError::Database(inner) => sqlx_kind(inner),
            StorageError::Io(io) => io_kind(io),
            StorageError::NotFound(_) => ProblemKind::Other,
        };
        Self::new(kind, err.to_string(), Some(db_path.to_path_buf()))
    }

    /// A failure creating or writing the folder at `path`.
    pub(crate) fn from_io(err: &std::io::Error, path: &Path) -> Self {
        Self::new(io_kind(err), format!("{}: {err}", path.display()), Some(path.to_path_buf()))
    }

    pub(crate) fn title(&self) -> String {
        match self.kind {
            ProblemKind::NeedsNewerApp => tr("This version of Audiobooklet is too old for your library"),
            ProblemKind::UpgradeFailed => tr("Couldn't update your library to this version"),
            ProblemKind::OutOfSpace => tr("Your device is out of storage space"),
            ProblemKind::NoAccess => tr("Audiobooklet can't write to its data folder"),
            ProblemKind::InUse => tr("Audiobooklet's library is in use"),
            ProblemKind::Damaged => tr("Audiobooklet's library is damaged"),
            ProblemKind::Other => tr("Audiobooklet couldn't start"),
        }
    }

    pub(crate) fn description(&self, install: InstallKind) -> String {
        let path = self.path.as_ref().map(|path| path.display().to_string()).unwrap_or_default();
        match self.kind {
            ProblemKind::NeedsNewerApp => {
                let what = tr("A newer version of Audiobooklet on this device — an AppImage next to the installed app, for example — last opened your library and changed it in a way this version doesn't understand. Update Audiobooklet to the latest version to keep using it; nothing has been lost.");
                let how = match install {
                    InstallKind::AppImage => tr("Download the latest AppImage and run it instead of this one."),
                    InstallKind::Flatpak => tr("Update it from your software center, or run “flatpak update”."),
                    InstallKind::Package => tr("Update it from your software center or package manager, or download the latest release."),
                };
                format!("{what}\n\n{how}")
            }
            ProblemKind::UpgradeFailed => tr("Something went wrong while bringing your library up to this version of the app. Your library hasn't been deleted. Try again; if it keeps happening, please report the details below."),
            ProblemKind::OutOfSpace => tr("Audiobooklet needs a little free space to open its library. Free up some space on the device, then try again."),
            // TRANSLATORS: {path} is a folder or file on the device, e.g. "/home/user/.local/share/…".
            ProblemKind::NoAccess => tr_args("Audiobooklet needs to write to {path}. Check that it belongs to you and isn't read-only, then try again.", &[("path", &path)]),
            ProblemKind::InUse => tr("Another copy of Audiobooklet may be running — possibly a different version of it. Close it, then try again."),
            // TRANSLATORS: {path} is the database file on the device.
            ProblemKind::Damaged => tr_args("Its local database, {path}, can't be read. You can start over: the damaged file is kept next to it, and you sign in again. Downloaded books stay on the device but need downloading again to appear.", &[("path", &path)]),
            ProblemKind::Other => tr("Something went wrong while opening the app's local data. Try again; if it keeps happening, please report the details below."),
        }
    }

    /// Whether "Try again" can help: everything but a too-old app, which only an update fixes.
    pub(crate) fn can_retry(&self) -> bool {
        self.kind != ProblemKind::NeedsNewerApp
    }
}

fn sqlx_kind(err: &sqlx::Error) -> ProblemKind {
    match err {
        sqlx::Error::Database(db) => match db.code().and_then(|code| code.parse::<i64>().ok()).map(|code| code & 0xff) {
            Some(SQLITE_FULL) => ProblemKind::OutOfSpace,
            Some(SQLITE_PERM | SQLITE_READONLY | SQLITE_CANTOPEN) => ProblemKind::NoAccess,
            Some(SQLITE_BUSY | SQLITE_LOCKED) => ProblemKind::InUse,
            Some(SQLITE_CORRUPT | SQLITE_NOTADB) => ProblemKind::Damaged,
            _ => ProblemKind::Other,
        },
        sqlx::Error::Io(io) => io_kind(io),
        sqlx::Error::PoolTimedOut => ProblemKind::InUse,
        _ => ProblemKind::Other,
    }
}

fn io_kind(err: &std::io::Error) -> ProblemKind {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::StorageFull => ProblemKind::OutOfSpace,
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => ProblemKind::NoAccess,
        _ => ProblemKind::Other,
    }
}

/// Whether SQLite really can't read the database at `path`: a fresh read-only connection runs
/// `PRAGMA quick_check`, and only "not a database", "corrupt" or a failed check count. A file
/// that opens and checks out is never offered "Start over", whatever the first error said.
pub(crate) async fn confirm_damaged(path: &Path) -> bool {
    use sqlx::{ConnectOptions, Row};
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(path).read_only(true);
    let mut connection = match options.connect().await {
        Ok(connection) => connection,
        Err(err) => return sqlx_kind(&err) == ProblemKind::Damaged,
    };
    let verdict = match sqlx::query("PRAGMA quick_check(1)").fetch_one(&mut connection).await {
        Ok(row) => row.try_get::<String, _>(0).map_or(true, |result| result != "ok"),
        Err(err) => sqlx_kind(&err) == ProblemKind::Damaged,
    };
    let _ = sqlx::Connection::close(connection).await;
    verdict
}

/// Moves the database at `db_path` aside (with its `-wal` and `-shm` companions) under a
/// `.damaged-<time>` suffix, so the next [`open`] starts a new one. Nothing is deleted. Returns
/// where the database went.
pub(crate) fn move_damaged_database(db_path: &Path) -> std::io::Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let aside = |path: &Path| PathBuf::from(format!("{}.damaged-{stamp}", path.display()));
    for suffix in ["-wal", "-shm"] {
        let companion = PathBuf::from(format!("{}{suffix}", db_path.display()));
        if companion.exists() {
            std::fs::rename(&companion, aside(&companion))?;
        }
    }
    let moved = aside(db_path);
    std::fs::rename(db_path, &moved)?;
    Ok(moved)
}

/// Opens the app's local data: directories, the database (migrated), and what the first window
/// needs from it. Every failure comes back as a [`StartupProblem`] to show, never a panic.
pub(crate) async fn open(paths: abs_storage::AppPaths) -> Result<AppState, StartupProblem> {
    let db_path = paths.db_path();
    if let Err(err) = paths.ensure_dirs().await {
        return Err(StartupProblem::from_io(&err, paths.data_dir()));
    }
    let storage = |err: StorageError| StartupProblem::from_storage(&err, &db_path);

    let pool = match abs_storage::connect_and_migrate(&db_path).await {
        Ok(pool) => pool,
        Err(err) => {
            let mut problem = storage(err);
            if problem.kind == ProblemKind::Damaged && !confirm_damaged(&db_path).await {
                problem.kind = ProblemKind::Other;
            }
            return Err(problem);
        }
    };

    // Low memory mode decides how the database itself is opened (fewer connections, a small page
    // cache), so it is read first, with the default pool, and the pool reopened if it is on. The
    // cover widgets get the flag before any window exists.
    let low_memory_mode = abs_core::settings::load_low_memory_mode(&pool).await.unwrap_or(false);
    tracing::info!(low_memory_mode, "loaded the low memory mode setting");
    let pool = if low_memory_mode {
        pool.close().await;
        tracing::info!("low memory mode: opening the database with fewer connections and a small page cache");
        abs_storage::connect_and_migrate_with(&db_path, abs_storage::DbProfile::LowMemory).await.map_err(storage)?
    } else {
        pool
    };
    crate::widgets::cover_image::set_low_memory_mode(low_memory_mode);

    // Stored download and cover paths still point into the old name's directories after they
    // moved (see `migrate_legacy_dirs` in `main`). Cheap and idempotent, so it runs on every start:
    // a crash between the move and this can't leave files the database no longer finds.
    for (from, to) in paths.legacy_pairs() {
        match abs_storage::repo::relocate::rebase_paths(&pool, &from, &to).await {
            Ok(0) => {}
            Ok(rows) => tracing::info!(rows, from = %from.display(), to = %to.display(), "pointed stored file paths at the moved data"),
            Err(err) => tracing::error!(%err, from = %from.display(), "couldn't update stored file paths after the move; downloads there may look missing"),
        }
    }

    // The language comes from the database, so it is applied here rather than at the top of
    // `main`: after the database is open, but before GTK starts (GTK reads the same locale) and
    // before any UI string is built. Never fails — the worst case is an English UI. (After a
    // startup problem, the system language was applied already, and this applies the setting.)
    let language = abs_core::settings::load_language(&pool).await.unwrap_or_else(|_| crate::i18n::SYSTEM.to_string());
    crate::i18n::init(&language);

    // Log anonymization is on from process start (see `log_privacy`); this applies the stored
    // choice and teaches the scrubber the addresses/usernames already in the database.
    let anonymize_logs = abs_core::settings::load_anonymize_logs(&pool).await.unwrap_or(true);
    crate::log_privacy::refresh_from_db(&pool).await;
    crate::log_privacy::global().set_enabled(anonymize_logs);
    tracing::info!(anonymize_logs, "loaded the log anonymization setting");

    let playback_settings = abs_core::settings::load_playback_settings(&pool).await.map_err(storage)?;
    tracing::info!(?playback_settings, "loaded playback settings");
    let theme = abs_core::settings::load_theme(&pool).await.map_err(storage)?;

    // A database left with accounts but none active (signing out of the active account used to
    // do that) starts on the most recently used one instead of the first-run screen.
    if let Err(err) = abs_core::accounts::ensure_active(&pool).await {
        tracing::warn!(%err, "couldn't pick an account to start with");
    }
    let active_account = abs_storage::repo::accounts::get_active(&pool).await.map_err(storage)?;
    tracing::info!(has_active_account = active_account.is_some(), "resolved startup screen");
    // Token freshness is handled lazily by `abs_core::auth::Session` — screens ask it for a
    // token at call time, and its first use refreshes an expired one (server v2.26.0+ JWTs).

    Ok(AppState { pool, paths, active_account, playback_settings, theme })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_error(code: i64) -> sqlx::Error {
        // A real SQLite error carrying `code`, from a statement that raises it on purpose is
        // impractical for most codes; `sqlx_kind` only reads `code()`, so a stand-in suffices.
        #[derive(Debug)]
        struct Fake(i64);
        impl std::fmt::Display for Fake {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "sqlite error {}", self.0)
            }
        }
        impl std::error::Error for Fake {}
        impl sqlx::error::DatabaseError for Fake {
            fn message(&self) -> &str {
                "fake"
            }
            fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
                Some(self.0.to_string().into())
            }
            fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
                self
            }
            fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
                self
            }
            fn kind(&self) -> sqlx::error::ErrorKind {
                sqlx::error::ErrorKind::Other
            }
        }
        sqlx::Error::Database(Box::new(Fake(code)))
    }

    fn kind_of(err: StorageError) -> ProblemKind {
        StartupProblem::from_storage(&err, Path::new("/data/db.sqlite3")).kind
    }

    /// A database with a migration this binary doesn't have — what a newer copy leaves behind.
    #[tokio::test]
    async fn a_database_from_a_newer_version_needs_a_newer_app_and_is_not_damaged() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("db.sqlite3");
        let pool = abs_storage::connect_and_migrate(&path).await.unwrap();
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (9999, 'from the future', 1, x'00', 0)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let err = abs_storage::connect_and_migrate(&path).await.unwrap_err();
        let problem = StartupProblem::from_storage(&err, &path);
        assert_eq!(problem.kind, ProblemKind::NeedsNewerApp, "{err}");
        assert!(!problem.can_retry(), "only an update helps");
        assert!(!confirm_damaged(&path).await, "a newer database is a healthy database");
    }

    #[tokio::test]
    async fn only_an_unreadable_file_is_damaged() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("db.sqlite3");
        std::fs::write(&path, vec![0x5a; 8192]).unwrap();
        let err = abs_storage::connect_and_migrate(&path).await.unwrap_err();
        assert_eq!(kind_of(err), ProblemKind::Damaged);
        assert!(confirm_damaged(&path).await);

        // A good database is never confirmed as damaged.
        let good = tmp.path().join("good.sqlite3");
        abs_storage::connect_and_migrate(&good).await.unwrap().close().await;
        assert!(!confirm_damaged(&good).await);
    }

    #[test]
    fn a_failed_upgrade_is_never_damaged_even_when_sqlite_says_corrupt() {
        assert_eq!(kind_of(StorageError::Migration(MigrateError::ExecuteMigration(db_error(SQLITE_CORRUPT), 12))), ProblemKind::UpgradeFailed);
        assert_eq!(kind_of(StorageError::Migration(MigrateError::Execute(db_error(SQLITE_NOTADB)))), ProblemKind::UpgradeFailed);
        assert_eq!(kind_of(StorageError::Migration(MigrateError::VersionMismatch(12))), ProblemKind::UpgradeFailed);
        assert_eq!(kind_of(StorageError::Migration(MigrateError::Dirty(12))), ProblemKind::UpgradeFailed);
        // Space, access and locks still say what they are while migrating.
        assert_eq!(kind_of(StorageError::Migration(MigrateError::ExecuteMigration(db_error(SQLITE_FULL), 12))), ProblemKind::OutOfSpace);
    }

    #[test]
    fn sqlite_and_io_errors_map_to_what_the_user_can_do() {
        assert_eq!(kind_of(StorageError::Database(db_error(SQLITE_FULL))), ProblemKind::OutOfSpace);
        assert_eq!(kind_of(StorageError::Database(db_error(SQLITE_READONLY))), ProblemKind::NoAccess);
        assert_eq!(kind_of(StorageError::Database(db_error(SQLITE_CANTOPEN))), ProblemKind::NoAccess);
        // An extended code (SQLITE_BUSY_RECOVERY) maps by its primary code.
        assert_eq!(kind_of(StorageError::Database(db_error(SQLITE_BUSY | (1 << 8)))), ProblemKind::InUse);
        assert_eq!(kind_of(StorageError::Database(db_error(SQLITE_CORRUPT))), ProblemKind::Damaged);
        assert_eq!(kind_of(StorageError::Database(db_error(1))), ProblemKind::Other);
        assert_eq!(kind_of(StorageError::Database(sqlx::Error::PoolTimedOut)), ProblemKind::InUse);
        let io = |kind| StartupProblem::from_io(&std::io::Error::from(kind), Path::new("/data")).kind;
        assert_eq!(io(std::io::ErrorKind::StorageFull), ProblemKind::OutOfSpace);
        assert_eq!(io(std::io::ErrorKind::PermissionDenied), ProblemKind::NoAccess);
        assert_eq!(io(std::io::ErrorKind::ReadOnlyFilesystem), ProblemKind::NoAccess);
        assert_eq!(io(std::io::ErrorKind::Other), ProblemKind::Other);
    }

    #[test]
    fn the_install_kind_decides_the_update_advice() {
        assert_eq!(InstallKind::from_env(true, false), InstallKind::AppImage);
        assert_eq!(InstallKind::from_env(false, true), InstallKind::Flatpak);
        assert_eq!(InstallKind::from_env(false, false), InstallKind::Package);
        let problem = StartupProblem::new(ProblemKind::NeedsNewerApp, "migration 12 was previously applied but is missing", None);
        assert!(problem.description(InstallKind::AppImage).contains("latest AppImage"));
        assert!(problem.description(InstallKind::Flatpak).contains("flatpak update"));
        assert!(problem.description(InstallKind::Package).contains("package manager"));
    }

    #[test]
    fn a_damaged_database_is_moved_aside_with_its_companions_never_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("db.sqlite3");
        for (name, content) in [("db.sqlite3", "main"), ("db.sqlite3-wal", "wal"), ("db.sqlite3-shm", "shm")] {
            std::fs::write(tmp.path().join(name), content).unwrap();
        }
        let moved = move_damaged_database(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "main");
        let names: Vec<String> = std::fs::read_dir(tmp.path()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.iter().filter(|name| name.contains(".damaged-")).count(), 3, "{names:?}");
    }
}
