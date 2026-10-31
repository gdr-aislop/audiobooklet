//! GStreamer's initialisation runs on its own thread, started just before GTK (see
//! `abs_player::init_in_background`), so its plugin-registry load overlaps the work the window
//! needs instead of delaying it. The first thing that needs GStreamer — building the player — waits
//! for it here, without blocking the main loop.

use adw::glib;
use std::sync::OnceLock;
use std::time::Duration;

static STARTED: OnceLock<abs_player::GstInit> = OnceLock::new();

/// Starts initialising GStreamer in the background; later calls do nothing.
pub(crate) fn start() {
    STARTED.get_or_init(|| {
        abs_player::init_in_background(|took, outcome| match outcome {
            Ok(()) => tracing::info!(took_ms = took.as_millis() as u64, "GStreamer initialised"),
            Err(err) => {
                tracing::warn!(%err, took_ms = took.as_millis() as u64, "GStreamer failed to initialize; playback will be unavailable")
            }
        })
    });
}

/// Resolves once GStreamer is initialised. Starts the initialisation if nothing has (the scenario
/// tests, which initialise it themselves, and so find it already done). A failure is logged by the
/// initialising thread; the player then falls back to its silent backend, as before.
///
/// While it takes unusually long it says so in the log (5 s, then every 10 s), since the window
/// would otherwise just sit on its loading content. It never gives up early: building the player
/// while GStreamer is still initialising is exactly what the wait prevents.
pub(crate) async fn wait() {
    start();
    let init = STARTED.get().expect("started just above").clone();
    wait_for(&init, Duration::from_secs(5), Duration::from_secs(10)).await;
}

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
        tracing::info!(waited_ms, "the main window waited for GStreamer");
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
