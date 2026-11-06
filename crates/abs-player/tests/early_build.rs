//! Building the player while GStreamer is still scanning its plugins.
//!
//! Its own test binary: the plugin registry file is chosen by `GST_REGISTRY`, which only counts
//! before GStreamer's first initialisation in the process, and an empty registry is what makes the
//! initialisation scan every plugin (seconds on a phone).

use abs_player::{init_in_background, GstBackend};

#[test]
fn a_player_built_during_the_plugin_scan_waits_for_it_instead_of_missing_playbin() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("GST_REGISTRY", tmp.path().join("fresh-registry.bin"));
    // No scanner subprocess: the scan runs in-process, on the initialisation thread.
    std::env::set_var("GST_REGISTRY_FORK", "no");

    let init = init_in_background(|_, _| {});
    // GStreamer's own flag turns true when the initialisation starts, long before the scan ends:
    // the player was once built the moment it did, and `playbin` was not registered yet.
    while !abs_player::is_initialized() && !init.is_finished() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let backend = GstBackend::new_with_sink("fakesink");
    assert!(backend.is_ok(), "the player must be buildable as soon as GStreamer says it is initialised: {:?}", backend.err());
    assert!(init.is_finished(), "building it waited for the scan to be done");
}
