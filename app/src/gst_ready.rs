//! GStreamer's initialisation runs on its own thread, started just before GTK (see
//! `abs_player::init_in_background`), so its plugin-registry load overlaps the work the window
//! needs instead of delaying it. The first thing that needs GStreamer — building the player — waits
//! for it here, without blocking the main loop.

use adw::glib;
use std::sync::OnceLock;
use std::time::Duration;

static STARTED: OnceLock<abs_player::GstInit> = OnceLock::new();

/// What the GStreamer plugin registry cache looked like at one moment. Logged before and after the
/// initialisation: a file that was missing, or rewritten by it, means the plugins were scanned
/// (seconds on the phone); one that was already there and unchanged means they were not.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryState {
    exists: bool,
    size_bytes: u64,
    modified: Option<std::time::SystemTime>,
}

impl RegistryState {
    fn of(path: Option<&std::path::Path>) -> Self {
        match path.and_then(|path| std::fs::metadata(path).ok()) {
            Some(meta) => Self { exists: true, size_bytes: meta.len(), modified: meta.modified().ok() },
            None => Self { exists: false, size_bytes: 0, modified: None },
        }
    }

    /// Whether `after` is not the file `self` was: it appeared, or its size or time changed.
    fn rewritten_by(&self, after: &Self) -> bool {
        self != after
    }

    fn age_s(&self) -> Option<u64> {
        self.modified.and_then(|at| at.elapsed().ok()).map(|age| age.as_secs())
    }
}

/// The registry file GStreamer will use: `GST_REGISTRY` when set (the AppImage launcher does),
/// else the newest `registry.*.bin` in the default cache folder. The second value says which, for
/// the log, which never carries the path itself (it holds the home directory).
fn registry_file() -> (Option<std::path::PathBuf>, &'static str) {
    if let Some(path) = std::env::var_os("GST_REGISTRY").filter(|path| !path.is_empty()) {
        return (Some(path.into()), "env");
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache")));
    let newest = cache.map(|cache| cache.join("gstreamer-1.0")).and_then(|dir| std::fs::read_dir(dir).ok()).and_then(|entries| {
        entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("registry.") && name.ends_with(".bin")
            })
            .max_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok())
            .map(|entry| entry.path())
    });
    (newest, "default")
}

/// Starts initialising GStreamer in the background; later calls do nothing.
pub(crate) fn start() {
    STARTED.get_or_init(|| {
        let (registry, source) = registry_file();
        let before = RegistryState::of(registry.as_deref());
        tracing::info!(
            source,
            exists = before.exists,
            size_bytes = before.size_bytes,
            age_s = before.age_s(),
            plugin_path_set = std::env::var_os("GST_PLUGIN_SYSTEM_PATH").is_some(),
            "GStreamer registry cache before the initialisation"
        );
        abs_player::init_in_background(move |took, outcome| {
            // The default location may not have existed before the first scan: look again.
            let registry_after = registry.or_else(|| registry_file().0);
            let after = RegistryState::of(registry_after.as_deref());
            tracing::info!(
                source,
                exists = after.exists,
                size_bytes = after.size_bytes,
                rewritten = before.rewritten_by(&after),
                "GStreamer registry cache after the initialisation"
            );
            match outcome {
                Ok(()) => tracing::info!(took_ms = took.as_millis() as u64, "GStreamer initialised"),
                Err(err) => {
                    tracing::warn!(%err, took_ms = took.as_millis() as u64, "GStreamer failed to initialize; playback will be unavailable")
                }
            }
        })
    });
}

#[cfg(test)]
thread_local! {
    /// Tests hold `is_ready()` false to see the shell before its audio is ready.
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test hook: while held, [`is_ready`] says no and [`on_ready`] callbacks wait.
#[cfg(test)]
pub(crate) fn hold_for_test(held: bool) {
    HELD.with(|flag| flag.set(held));
}

fn held() -> bool {
    #[cfg(test)]
    {
        HELD.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Whether the audio can be built right now: GStreamer finished initialising (or failed, in which
/// case the silent backend is what there is).
pub(crate) fn is_ready() -> bool {
    !held() && (abs_player::is_initialized() || STARTED.get().is_some_and(|init| init.is_finished()))
}

/// Runs `callback` on the main loop once [`is_ready`] — at once if it already is.
pub(crate) fn on_ready(callback: impl FnOnce() + 'static) {
    if is_ready() {
        callback();
        return;
    }
    start();
    let init = STARTED.get().expect("started just above").clone();
    glib::spawn_future_local(async move {
        wait_for(&init, Duration::from_secs(5), Duration::from_secs(10)).await;
        // Only loops while a test holds it.
        while !is_ready() && held() {
            glib::timeout_future(Duration::from_millis(20)).await;
        }
        callback();
    });
}

/// While it takes unusually long it says so in the log (5 s, then every 10 s). It never gives up
/// early: building the player while GStreamer is still initialising is exactly what waiting
/// prevents.
async fn wait_for(init: &abs_player::GstInit, first_warning: Duration, then_every: Duration) {
    let waited = std::time::Instant::now();
    let mut next_warning = first_warning;
    loop {
        let remaining = next_warning.saturating_sub(waited.elapsed());
        let timer = glib::timeout_future(remaining);
        match futures::future::select(Box::pin(init.wait()), timer).await {
            futures::future::Either::Left(_) => break,
            futures::future::Either::Right(_) => {
                tracing::warn!(waited_ms = waited.elapsed().as_millis() as u64, "still waiting for GStreamer to initialise");
                next_warning += then_every;
            }
        }
    }
    let waited_ms = waited.elapsed().as_millis() as u64;
    if waited_ms >= 20 {
        tracing::info!(waited_ms, "waited for GStreamer");
    }
}

/// For the end of `main`: lets an initialisation still running finish (up to `limit`) before the
/// process exits, so `exit()`'s destructors never run under a thread that is inside GStreamer.
pub(crate) fn wait_blocking(limit: Duration) {
    let Some(init) = STARTED.get() else { return };
    let begun = std::time::Instant::now();
    while !init.is_finished() && begun.elapsed() < limit {
        std::thread::sleep(Duration::from_millis(20));
    }
    if begun.elapsed() >= Duration::from_millis(20) {
        tracing::info!(waited_ms = begun.elapsed().as_millis() as u64, finished = init.is_finished(), "waited for GStreamer before exiting");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_state_tells_missing_present_and_rewritten_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("registry.bin");
        let missing = RegistryState::of(Some(&file));
        assert!(!missing.exists && missing.size_bytes == 0 && missing.age_s().is_none());
        assert_eq!(RegistryState::of(None), missing, "no file known counts as missing");

        std::fs::write(&file, vec![0u8; 1000]).unwrap();
        let first = RegistryState::of(Some(&file));
        assert!(first.exists && first.size_bytes == 1000 && first.age_s().is_some());
        assert!(missing.rewritten_by(&first), "appearing is a (re)write: the plugins were scanned");
        assert!(!first.rewritten_by(&RegistryState::of(Some(&file))), "untouched: a cache hit");

        std::fs::write(&file, vec![0u8; 2000]).unwrap();
        assert!(first.rewritten_by(&RegistryState::of(Some(&file))), "a changed size is a rewrite");
    }

    #[test]
    fn the_registry_file_comes_from_the_environment_or_the_default_cache_and_is_never_logged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gstreamer-1.0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("registry.x86_64.bin"), b"x").unwrap();
        // Only this test touches these variables in the app crate's unit tests, and it runs before
        // any GTK scenario (those are separate processes).
        std::env::remove_var("GST_REGISTRY");
        std::env::set_var("XDG_CACHE_HOME", tmp.path());
        let (found, source) = registry_file();
        assert_eq!((found.as_deref(), source), (Some(dir.join("registry.x86_64.bin").as_path()), "default"));
        std::env::set_var("GST_REGISTRY", tmp.path().join("own.bin"));
        let (found, source) = registry_file();
        assert_eq!((found.as_deref(), source), (Some(tmp.path().join("own.bin").as_path()), "env"));
        std::env::remove_var("GST_REGISTRY");
    }

    #[test]
    fn a_slow_initialisation_is_warned_about_and_the_wait_still_ends_with_it() {
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let init = abs_player::init_in_background_with(
            move || {
                let _ = gate.recv_timeout(Duration::from_secs(10));
                Ok(())
            },
            |_, _| {},
        );
        let context = glib::MainContext::default();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            release.send(()).unwrap();
        });
        let started = std::time::Instant::now();
        context.block_on(wait_for(&init, Duration::from_millis(100), Duration::from_millis(100)));
        releaser.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(240), "it must not give up before the work is done");
        assert!(init.is_finished());
    }

    #[test]
    fn wait_blocking_returns_at_the_limit_while_the_work_runs_and_at_once_when_done() {
        // `STARTED` is process-wide: install a gated init, check, then let it finish.
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let init = abs_player::init_in_background_with(
            move || {
                let _ = gate.recv_timeout(Duration::from_secs(10));
                Ok(())
            },
            |_, _| {},
        );
        let _ = STARTED.set(init.clone());
        let started = std::time::Instant::now();
        wait_blocking(Duration::from_millis(150));
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(140) && took < Duration::from_millis(400), "{took:?}");
        assert!(!init.is_finished());
        release.send(()).unwrap();
        while !init.is_finished() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = std::time::Instant::now();
        wait_blocking(Duration::from_secs(3));
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
