//! GStreamer's initialisation runs on its own thread, started before the database opens (see
//! `abs_player::init_in_background`), so its plugin-registry load overlaps the work the window
//! needs instead of delaying it. The first thing that needs GStreamer — building the player — waits
//! for it here, without blocking the main loop.

use std::sync::OnceLock;

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
pub(crate) async fn wait() {
    start();
    let init = STARTED.get().expect("started just above").clone();
    let waited = std::time::Instant::now();
    let _ = init.wait().await;
    let waited_ms = waited.elapsed().as_millis() as u64;
    if waited_ms >= 20 {
        tracing::info!(waited_ms, "the main window waited for GStreamer");
    }
}
