mod a11y;
mod application;
mod crash_reporting;
mod downloads;
mod error_reporting;
mod external_apps;
mod i18n;
mod icons;
mod keep_awake;
mod listening;
mod log_privacy;
mod low_memory_mode;
mod offline_mode;
mod password_keyring;
mod perf;
mod player;
mod rich_text;
mod screens;
mod sync_coordinator;
#[cfg(test)]
mod test_support;
mod widgets;

use adw::prelude::*;
use application::AppState;

fn main() -> adw::glib::ExitCode {
    // Checked before *everything* else, including `--version` — a real user never passes this
    // flag; it exists only so this same binary can re-exec itself as the crash-dump IPC server
    // (see `crash_reporting`'s module doc comment). The spawned server must never run `setup()`,
    // touch the database, or init GStreamer/GTK, so this has to come first.
    if let Some(socket_name) = crash_reporting::crash_server_socket_name() {
        let paths = abs_storage::AppPaths::resolve().expect("resolve XDG application directories");
        crash_reporting::run_crash_server(&socket_name, paths.crash_dumps_dir());
    }

    // `--version` / `-v`: print and exit before *any* side effect — notably before setup()
    // resolves XDG paths and opens/migrates the user's database, which a version query has no
    // business doing. Parsed by hand here rather than via GApplication's option machinery
    // (`add_main_option` + `handle-local-options`) because that only runs inside `app.run()`,
    // after this file's eager `setup()` has already happened. Nothing below this point runs
    // for the flag: no Tokio runtime, no GStreamer, no disk access.
    if std::env::args().any(|arg| arg == "--version" || arg == "-v") {
        println!("{}", application::version_line());
        return 0.into();
    }

    // Resolved here, synchronously, rather than inside `setup()` — logging and the panic hook
    // below need `AppPaths` (specifically `logs_dir()`) before a Tokio runtime exists, so they
    // can also catch failures during the async setup that follows. `ensure_early_dirs` covers
    // only what's needed for that; `setup()`'s own `ensure_dirs()` call still creates every
    // directory (redundantly but harmlessly for these two).
    let paths = abs_storage::AppPaths::resolve().expect("resolve XDG application directories");
    // The app was called abs-app up to 0.9.5: move its directories over before anything creates
    // the new ones (logging, just below, would). Logged once logging is up.
    let legacy_moves = paths.migrate_legacy_dirs();
    paths.ensure_early_dirs().expect("create the logging/crash-dump directories");

    let log_handle = crash_reporting::init_logging(&paths);
    // One of the very first lines in every log, on purpose: a user's pasted log snippet should
    // be self-identifying — which build it came from, which OS/architecture, and where the rest
    // of this log file lives — without anyone having to ask.
    tracing::info!(
        version = %application::version_line(),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        state_dir = %paths.state_dir().display(),
        "starting up"
    );
    for outcome in &legacy_moves {
        match outcome {
            abs_storage::paths::LegacyDirMove::Moved { from, to } => {
                tracing::info!(from = %from.display(), to = %to.display(), "moved the data of the app's old name over")
            }
            abs_storage::paths::LegacyDirMove::BothExist { legacy, current } => tracing::warn!(
                legacy = %legacy.display(),
                current = %current.display(),
                "found the old name's directory next to the new one; using the new one and leaving the old one alone"
            ),
            abs_storage::paths::LegacyDirMove::Failed { from, to, error } => tracing::error!(
                from = %from.display(),
                to = %to.display(),
                %error,
                "couldn't move the data of the app's old name over; starting without it"
            ),
        }
    }
    crash_reporting::install_panic_hook(log_handle.clone());
    // Failure here degrades to a warning inside `attach_crash_handler` itself — crash-dump
    // capture must never block the app from starting (e.g. under a sandboxed/seccomp environment
    // that blocks spawning a subprocess or installing a signal handler).
    let _crash_client = crash_reporting::attach_crash_handler();

    // One shared Tokio runtime for the whole app (see the architecture plan's async-runtime
    // note): GTK runs on GLib's main loop, but abs-api/abs-storage are async. Setup that must
    // finish before the first window appears runs here with `block_on`; screens making live
    // network/DB calls afterwards (e.g. the Welcome screen's Connect button) spawn onto this same
    // runtime via `glib::spawn_future_local` instead.
    //
    // `_runtime_guard` MUST stay alive for the rest of `main` (not just across `block_on` above):
    // `enter()` sets the "current" Tokio runtime for whichever thread calls it, and
    // `spawn_future_local`-driven futures run on the GTK main thread, not inside `block_on`. Once
    // the guard is dropped, sqlx/reqwest calls made from those futures panic with "this
    // functionality requires a Tokio context" — caught by hand-driving the real Connect button
    // under Xvfb, since the widget test suite builds its own runtime/guard directly and never
    // exercises this file's setup.
    let runtime = tokio::runtime::Runtime::new().expect("build the Tokio runtime");
    let _runtime_guard = runtime.enter();
    let state = runtime.block_on(setup(paths));

    if let Err(err) = abs_player::init() {
        tracing::warn!(%err, "GStreamer failed to initialize; playback will be unavailable");
    }

    let app = application::build_application(state);
    let exit_code = app.run();
    // While thread-local storage is still intact: the shell's teardown (the player retiring)
    // must not run inside `exit()`'s thread-local destructors.
    application::release_main_window();
    log_handle.flush();
    exit_code
}

async fn setup(paths: abs_storage::AppPaths) -> AppState {
    paths.ensure_dirs().await.expect("create application directories");

    let pool = abs_storage::connect_and_migrate(&paths.db_path())
        .await
        .expect("open and migrate the local database");

    // Low memory mode decides how the database itself is opened (fewer connections, a small page
    // cache), so it is read first, with the default pool, and the pool reopened if it is on. The
    // cover widgets get the flag before any window exists.
    let low_memory_mode = abs_core::settings::load_low_memory_mode(&pool).await.unwrap_or(false);
    tracing::info!(low_memory_mode, "loaded the low memory mode setting");
    let pool = if low_memory_mode {
        pool.close().await;
        tracing::info!("low memory mode: opening the database with fewer connections and a small page cache");
        abs_storage::connect_and_migrate_with(&paths.db_path(), abs_storage::DbProfile::LowMemory)
            .await
            .expect("open the local database in low memory mode")
    } else {
        pool
    };
    widgets::cover_image::set_low_memory_mode(low_memory_mode);

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
    // before any UI string is built. Never fails — the worst case is an English UI.
    let language = abs_core::settings::load_language(&pool).await.unwrap_or_else(|_| i18n::SYSTEM.to_string());
    i18n::init(&language);

    // Log anonymization is on from process start (see `log_privacy`); this applies the stored
    // choice and teaches the scrubber the addresses/usernames already in the database.
    let anonymize_logs = abs_core::settings::load_anonymize_logs(&pool).await.unwrap_or(true);
    log_privacy::refresh_from_db(&pool).await;
    log_privacy::global().set_enabled(anonymize_logs);
    tracing::info!(anonymize_logs, "loaded the log anonymization setting");

    let playback_settings = abs_core::settings::load_playback_settings(&pool)
        .await
        .expect("load playback settings");
    tracing::info!(?playback_settings, "loaded playback settings");
    let theme = abs_core::settings::load_theme(&pool).await.expect("load theme");

    // A database left with accounts but none active (signing out of the active account used to
    // do that) starts on the most recently used one instead of the first-run screen.
    if let Err(err) = abs_core::accounts::ensure_active(&pool).await {
        tracing::warn!(%err, "couldn't pick an account to start with");
    }
    let active_account = abs_storage::repo::accounts::get_active(&pool)
        .await
        .expect("check for an active account");
    tracing::info!(
        has_active_account = active_account.is_some(),
        "resolved startup screen"
    );
    // Token freshness is handled lazily by `abs_core::auth::Session` — screens ask it for a
    // token at call time, and its first use refreshes an expired one (server v2.26.0+ JWTs).

    AppState { pool, paths, active_account, playback_settings, theme }
}

/// Every GTK-touching test across `screens::*` funnels through this module. `gtk4::init()` binds
/// to whichever OS thread first calls it, and Rust's built-in test harness gives every `#[test]`
/// fn its own fresh OS thread even under `--test-threads=1` (that flag only limits how many run
/// *concurrently*, not which thread each runs on) — so two `#[test]` fns that both call
/// `gtk4::init()` panic with "Attempted to initialize GTK from two different threads", regardless
/// of file. Each screen module therefore keeps its own test code local (`pub(crate) mod tests`
/// with plain `run_*` functions, not `#[test]`s) for readability; only the plumbing lives here.
///
/// The plumbing used to be one giant `#[test]` calling every scenario in sequence. That made the
/// suite all-or-nothing: a single flaky assert failed the whole ~45 s binary, and isolating one
/// scenario meant hand-editing this registry. Instead, every scenario below is generated as its
/// own `#[ignore]`d `#[test]`, and the one test cargo runs by default (`gtk_fast_scenarios`)
/// re-execs *this test binary* once per scenario with `--exact --ignored` — so each scenario gets
/// a fresh process (its own GTK init, main context, GStreamer, tempdirs), a named pass/fail, and
/// its failure name doubles as the command to re-run it in isolation:
///
///     cargo test -p audiobooklet -- --exact tests::home_offline_mode_toggle_filters_recently_added --ignored
///
/// (The per-scenario tests can't run under a plain `cargo test` themselves — that's a second
/// GTK init on a second thread; they exist to be picked one-per-process by the driver or by hand.)
#[cfg(test)]
mod tests {
    /// Boots one scenario in "a process that owns GTK": init, then a shared Tokio runtime whose
    /// thread-local guard must outlive the scenario (see `main`'s runtime-guard comment — same
    /// requirement, same failure mode if dropped early: sqlx/reqwest calls made from
    /// `glib::spawn_future_local` futures panic with "requires a Tokio context").
    fn run_scenario(scenario: fn(&tokio::runtime::Runtime)) {
        gtk4::init().expect("gtk4::init for the scenario process");
        crate::icons::register(&gtk4::gdk::Display::default().expect("gtk4::init opened a display"));
        abs_player::init().expect("gstreamer::init for the scenario process");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        crate::i18n::audit::start();
        scenario(&runtime);
        crate::i18n::audit::finish(&std::thread::current().name().unwrap_or("scenario").to_string());
    }

    /// One entry per scenario: `(name, closure)`. The closure receives the shared `&Runtime`
    /// (scenarios that don't need one ignore it as `_rt`). `name` becomes the generated
    /// `#[test]` fn and the `tests::<name>` handle both the driver and manual re-runs use —
    /// keep names unique and module-prefixed, since several screens share scenario tails like
    /// "…shows_empty_state_when_the_server_has_no_libraries".
    macro_rules! gtk_scenarios {
        ($(($name:ident, $call:expr)),* $(,)?) => {
            $(
                #[test]
                #[ignore = "per-scenario process: run via the gtk_fast_scenarios driver, or --exact --ignored tests::<name>"]
                fn $name() {
                    run_scenario($call);
                }
            )*

            /// Walked by the `gtk_fast_scenarios` driver; kept in registry order for readable output.
            pub(crate) const GTK_SCENARIOS: &[&str] = &[$(stringify!($name)),*];
        };
    }

    gtk_scenarios! {
        (a11y_helpers, crate::a11y::tests::run_helpers),
        (application_single_key_shortcuts_defer_to_the_focused_widget, crate::application::tests::run_single_key_shortcuts_defer_to_the_focused_widget),
        (application_activating_again_presents_the_open_window, crate::application::tests::run_activating_again_presents_the_open_window),
        (error_reporting_shows_one_toast, crate::error_reporting::tests::run_report_background_error_shows_one_toast),
        (icons_every_icon_the_app_uses_is_in_the_theme, crate::icons::tests::run_every_icon_the_app_uses_is_in_the_theme),
        (welcome_connect, crate::screens::welcome::tests::run),
        (welcome_relogin_flow, crate::screens::welcome::tests::run_relogin_flow),
        (welcome_api_key_and_advanced, crate::screens::welcome::tests::run_api_key_and_advanced),
        (home_renders_synced_library_and_recently_added_item, crate::screens::home::tests::run_renders_synced_library_and_recently_added_item),
        (home_finished_books_dont_show_under_continue_listening, crate::screens::home::tests::run_finished_books_dont_show_under_continue_listening),
        (home_shelf_headers_invoke_on_open_shelf, crate::screens::home::tests::run_shelf_headers_invoke_on_open_shelf),
        (home_shows_empty_state_when_the_server_has_no_libraries, crate::screens::home::tests::run_shows_empty_state_when_the_server_has_no_libraries),
        (home_shows_a_retryable_error_when_the_first_sync_fails, crate::screens::home::tests::run_shows_a_retryable_error_when_the_first_sync_fails),
        (home_offline_mode_toggle_filters_recently_added, crate::screens::home::tests::run_offline_mode_toggle_filters_recently_added),
        (home_shows_a_spinner_while_the_first_sync_is_running, crate::screens::home::tests::run_shows_a_spinner_while_the_first_sync_is_running),
        (home_expired_token_is_refreshed_before_syncing, crate::screens::home::tests::run_expired_token_is_refreshed_before_syncing),
        (home_offers_login_again_when_the_session_is_rejected, crate::screens::home::tests::run_offers_login_again_when_the_session_is_rejected),
        (home_shows_login_again_in_the_banner_when_a_resync_is_rejected, crate::screens::home::tests::run_shows_login_again_in_the_banner_when_a_resync_is_rejected),
        (home_sync_now_resyncs_and_toasts, crate::screens::home::tests::run_sync_now_resyncs_and_toasts),
        (home_pull_to_refresh_resyncs_and_toasts, crate::screens::home::tests::run_pull_to_refresh_resyncs_and_toasts),
        (home_sync_now_toasts_failure, crate::screens::home::tests::run_sync_now_toasts_failure),
        (library_renders_all_synced_items, crate::screens::library::tests::run_renders_all_synced_items),
        (library_a_load_logs_each_step, crate::screens::library::tests::run_a_library_load_logs_each_step),
        (library_search_filters_by_title_and_author, crate::screens::library::tests::run_search_filters_by_title_and_author),
        (library_search_ignores_national_characters, crate::screens::library::tests::run_search_ignores_national_characters),
        (library_sort_changes_order, crate::screens::library::tests::run_sort_changes_order),
        (library_sorts_by_last_listened, crate::screens::library::tests::run_sorts_by_last_listened),
        (library_in_progress_filter_is_manually_toggleable, crate::screens::library::tests::run_in_progress_filter_is_manually_toggleable),
        (library_offline_mode_toggle_filters_to_downloaded_items, crate::screens::library::tests::run_offline_mode_toggle_filters_to_downloaded_items),
        (library_empty_states_follow_offline_mode_and_search, crate::screens::library::tests::run_empty_states_follow_offline_mode_and_search),
        (library_shows_a_banner_when_sync_fails, crate::screens::library::tests::run_shows_a_banner_when_sync_fails),
        (library_shows_login_again_in_the_banner_when_a_resync_is_rejected, crate::screens::library::tests::run_shows_login_again_in_the_banner_when_a_resync_is_rejected),
        (library_shows_empty_state_when_the_server_has_no_libraries, crate::screens::library::tests::run_shows_empty_state_when_the_server_has_no_libraries),
        (library_tapping_a_card_invokes_on_open, crate::screens::library::tests::run_tapping_a_card_invokes_on_open),
        (library_list_view_toggle_switches_visible_container, crate::screens::library::tests::run_list_view_toggle_switches_visible_container),
        (library_switching_view_mode_shows_a_spinner_immediately, crate::screens::library::tests::run_switching_view_mode_shows_a_spinner_immediately),
        (library_category_chip_shows_a_spinner_while_regrouping, crate::screens::library::tests::run_category_chip_shows_a_spinner_while_regrouping),
        (library_list_view_rows_show_title_and_subtitle, crate::screens::library::tests::run_list_view_rows_show_title_and_subtitle),
        (library_tapping_a_list_row_invokes_on_open, crate::screens::library::tests::run_tapping_a_list_row_invokes_on_open),
        (library_search_and_sort_apply_in_list_mode_too, crate::screens::library::tests::run_search_and_sort_apply_in_list_mode_too),
        (library_view_mode_is_remembered_across_screen_rebuilds, crate::screens::library::tests::run_view_mode_is_remembered_across_screen_rebuilds),
        (library_a_big_library_renders_in_slices_and_skips_identical_renders, crate::screens::library::tests::run_a_big_library_renders_in_slices_and_skips_identical_renders),
        (library_low_memory_mode_and_view_switches_release_what_is_not_shown, crate::screens::library::tests::run_low_memory_mode_and_view_switches_release_what_is_not_shown),
        (library_downloaded_badges_follow_downloads_in_this_session, crate::screens::library::tests::run_downloaded_badges_follow_downloads_in_this_session),
        (home_downloaded_badges_follow_downloads_in_this_session, crate::screens::home::tests::run_downloaded_badges_follow_downloads_in_this_session),
        (home_offline_mode_makes_no_requests, crate::screens::home::tests::run_offline_mode_makes_no_requests),
        (home_low_memory_mode_hides_the_covers_on_the_shelves, crate::screens::home::tests::run_low_memory_mode_hides_the_covers_on_the_shelves),
        (home_missing_covers_are_asked_again_only_by_a_manual_sync, crate::screens::home::tests::run_missing_covers_are_asked_again_only_by_a_manual_sync),
        (playback_offline_mode_plays_and_saves_locally_then_catches_up, crate::player::tests::run_offline_mode_plays_and_saves_locally_then_catches_up),
        (playback_offline_mode_does_not_stream, crate::player::tests::run_offline_mode_does_not_stream),
        (downloads_engine_offline_mode_starts_no_download, crate::downloads::tests::run_offline_mode_starts_no_download),
        (library_search_is_debounced, crate::screens::library::tests::run_search_is_debounced),
        (library_deferred_decode_only_covers_items_near_the_viewport, crate::screens::library::tests::run_deferred_decode_only_covers_items_near_the_viewport),
        (library_hide_finished_switch_filters_finished_items, crate::screens::library::tests::run_hide_finished_switch_filters_finished_items),
        (library_author_category_chip_groups_items_with_headers, crate::screens::library::tests::run_author_category_chip_groups_items_with_headers),
        (library_series_grouping_sorts_named_groups_before_the_fallback_bucket, crate::screens::library::tests::run_series_grouping_sorts_named_groups_before_the_fallback_bucket),
        (library_apply_series_filter_shows_only_that_series, crate::screens::library::tests::run_apply_series_filter_shows_only_that_series),
        (library_author_grouping_sorts_named_authors_before_unknown_author_bucket, crate::screens::library::tests::run_author_grouping_sorts_named_authors_before_unknown_author_bucket),
        (library_sort_by_combo_persists_and_is_honored_on_rebuild, crate::screens::library::tests::run_sort_by_combo_persists_and_is_honored_on_rebuild),
        (library_last_listened_sort_persists_and_is_honored_on_rebuild, crate::screens::library::tests::run_last_listened_sort_persists_and_is_honored_on_rebuild),
        (library_in_progress_only_switch_persists_and_is_honored_on_rebuild, crate::screens::library::tests::run_in_progress_only_switch_persists_and_is_honored_on_rebuild),
        (library_genre_chip_filters_without_persisting, crate::screens::library::tests::run_genre_chip_filters_without_persisting),
        (library_application_settings_row_calls_on_open_settings, crate::screens::library::tests::run_application_settings_row_calls_on_open_settings),
        (library_sync_now_and_pull_to_refresh, crate::screens::library::tests::run_sync_now_and_pull_to_refresh),
        (item_detail_metadata_renders_from_cache_and_play_for_unstarted, crate::screens::item_detail::tests::run_metadata_renders_from_cache_and_play_for_unstarted),
        (item_detail_empty_narrator_falls_back_to_author_only, crate::screens::item_detail::tests::run_empty_narrator_falls_back_to_author_only),
        (item_detail_partially_listened_book_shows_resume_and_progress, crate::screens::item_detail::tests::run_partially_listened_book_shows_resume_and_progress),
        (item_detail_description_truncation_and_more_toggle, crate::screens::item_detail::tests::run_description_truncation_and_more_toggle),
        (item_detail_description_renders_html_as_markup, crate::screens::item_detail::tests::run_description_renders_html_as_markup),
        (item_detail_chapter_rows_show_offline_glyphs_and_seek_on_tap, crate::screens::item_detail::tests::run_chapter_rows_show_offline_glyphs_and_seek_on_tap),
        (item_detail_tapping_play_invokes_on_play_and_leaves_navigation_to_the_caller, crate::screens::item_detail::tests::run_tapping_play_invokes_on_play_and_leaves_navigation_to_the_caller),
        (item_detail_back_button_invokes_on_back, crate::screens::item_detail::tests::run_back_button_invokes_on_back),
        (item_detail_download_menu_starts_a_real_download, crate::screens::item_detail::tests::run_download_menu_starts_a_real_download),
        (item_detail_download_progress_strip_reveals_and_chapter_glyphs_update_live, crate::screens::item_detail::tests::run_download_progress_strip_reveals_and_chapter_glyphs_update_live),
        (item_detail_download_menu_shows_a_loading_placeholder_until_chapters_resolve, crate::screens::item_detail::tests::run_download_menu_shows_a_loading_placeholder_until_chapters_resolve),
        (item_detail_download_menu_uses_cached_chapters_immediately, crate::screens::item_detail::tests::run_download_menu_uses_cached_chapters_immediately),
        (item_detail_shows_the_mini_bar_for_whatever_is_currently_playing, crate::screens::item_detail::tests::run_shows_the_mini_bar_for_whatever_is_currently_playing),
        (item_detail_series_button_shows_the_plain_name_then_fills_in_the_real_numbers, crate::screens::item_detail::tests::run_series_button_shows_the_plain_name_then_fills_in_the_real_numbers),
        (item_detail_series_button_keeps_the_plain_name_when_the_network_call_fails, crate::screens::item_detail::tests::run_series_button_keeps_the_plain_name_when_the_network_call_fails),
        (item_detail_series_button_stays_hidden_when_the_item_has_no_series, crate::screens::item_detail::tests::run_series_button_stays_hidden_when_the_item_has_no_series),
        (item_detail_action_buttons_render_equal_heights, crate::screens::item_detail::tests::run_action_buttons_render_equal_heights),
        (item_detail_options_menu_marks_finished_via_direct_write_when_not_currently_playing, crate::screens::item_detail::tests::run_options_menu_marks_finished_via_direct_write_when_not_currently_playing),
        (item_detail_options_menu_direct_write_failure_toasts_the_failure, crate::screens::item_detail::tests::run_options_menu_direct_write_failure_toasts_the_failure),
        (item_detail_options_menu_resets_progress_via_direct_write_when_not_currently_playing, crate::screens::item_detail::tests::run_options_menu_resets_progress_via_direct_write_when_not_currently_playing),
        (item_detail_options_menu_reset_can_be_undone, crate::screens::item_detail::tests::run_options_menu_reset_can_be_undone),
        (item_detail_options_menu_delegates_to_the_live_controller_when_this_item_is_playing, crate::screens::item_detail::tests::run_options_menu_delegates_to_the_live_controller_when_this_item_is_playing),
        (item_card_renders, |_rt| crate::widgets::item_card::tests::run()),
        (item_card_wrap_title_shows_the_full_title_without_ellipsizing, |_rt| crate::widgets::item_card::tests::run_wrap_title_shows_the_full_title_without_ellipsizing()),
        (item_card_downloaded_badge_shows_only_when_downloaded, |_rt| crate::widgets::item_card::tests::run_downloaded_badge_shows_only_when_downloaded()),
        (item_card_progress_row_shows_only_when_given_progress, |_rt| crate::widgets::item_card::tests::run_progress_row_shows_only_when_given_progress()),
        (main_window_shell, crate::screens::main_window::tests::run),
        (main_window_offline_mode_toggle_is_shared_between_home_and_library, crate::screens::main_window::tests::run_offline_mode_toggle_is_shared_between_home_and_library),
        (main_window_call_interruption_wiring_pauses_playback, crate::screens::main_window::tests::run_call_interruption_wiring_pauses_playback),
        (main_window_connectivity_restored_wiring_syncs_pending_progress, crate::screens::main_window::tests::run_connectivity_restored_wiring_syncs_pending_progress),
        (main_window_headphone_route_events_follow_the_settings, crate::screens::main_window::tests::run_headphone_route_events_follow_the_settings),
        (main_window_tapping_a_card_opens_item_detail_then_play_opens_the_player_screen, crate::screens::main_window::tests::run_tapping_a_card_opens_item_detail_then_play_opens_the_player_screen),
        (main_window_playback_error_toasts_once_with_a_details_action, crate::screens::main_window::tests::run_playback_error_toasts_once_with_a_details_action),
        (main_window_tapping_the_series_button_opens_library_filtered_to_that_series, crate::screens::main_window::tests::run_tapping_the_series_button_opens_library_filtered_to_that_series),
        (main_window_download_started_toast_view_action_opens_downloads, crate::screens::main_window::tests::run_download_started_toast_view_action_opens_downloads),
        (main_window_mini_bar_open_player_swaps_to_the_full_player_screen, crate::screens::main_window::tests::run_mini_bar_open_player_swaps_to_the_full_player_screen),
        (main_window_mini_bar_screenshots, crate::screens::main_window::tests::run_mini_bar_screenshots),
        (banner_fits_a_phone_and_hides_an_empty_bottom_row, crate::widgets::banner::tests::run_the_banner_fits_a_phone_and_hides_an_empty_bottom_row),
        (main_window_server_down_at_launch_is_shown_on_home_and_library, crate::screens::main_window::tests::run_server_down_at_launch_is_shown_on_home_and_library),
        (main_window_server_down_on_a_fresh_install_is_shown_on_home, crate::screens::main_window::tests::run_server_down_on_a_fresh_install_is_shown_on_home),
        (main_window_play_with_the_server_down_says_it_cannot_start, crate::screens::main_window::tests::run_play_with_the_server_down_says_it_cannot_start),
        (main_window_every_screen_fits_a_phone, crate::screens::main_window::tests::run_every_screen_fits_a_phone),
        (settings_persistence, crate::screens::settings::tests::run),
        (settings_playback_defaults_theme_and_about, crate::screens::settings::tests::run_playback_defaults_theme_and_about),
        (settings_open_latest_log_row_reports_a_missing_log, crate::screens::settings::tests::run_open_latest_log_row_reports_a_missing_log),
        (settings_language_row_offers_system_and_english_and_persists, crate::screens::settings::tests::run_language_row_offers_system_and_english_and_persists),
        (settings_anonymize_logs_switch_drives_the_scrubber_and_persists, crate::screens::settings::tests::run_anonymize_logs_switch_drives_the_scrubber_and_persists),
        (settings_low_memory_mode_switch_persists_and_hints_at_burst_buffering, crate::screens::settings::tests::run_low_memory_mode_switch_persists_and_hints_at_burst_buffering),
        (settings_account_and_servers_rows_reflect_the_database, crate::screens::settings::tests::run_account_and_servers_rows_reflect_the_database),
        (settings_servers_menu_actions_rebuild_the_shell, crate::screens::settings::tests::run_servers_menu_actions_rebuild_the_shell),
        (settings_signing_out_of_the_active_account_lands_on_another, crate::screens::settings::tests::run_signing_out_of_the_active_account_lands_on_another),
        (settings_add_server_row_opens_welcome_and_cancels_back, crate::screens::settings::tests::run_add_server_row_opens_welcome_and_cancels_back),
        (settings_failed_setting_saves_toast, crate::screens::settings::tests::run_failed_setting_saves_toast),
        (offline_mode_failed_persist_is_reported, crate::offline_mode::tests::run_failed_persist_is_reported),
        (connection_page_url_info_disconnect_and_back, crate::screens::connection::tests::run_url_info_disconnect_and_back),
        (connection_advanced_rows_persist_and_edit, crate::screens::connection::tests::run_advanced_rows_persist_and_edit),
        (connection_editor_persist_failures_show_inline, crate::screens::connection::tests::run_editor_persist_failures_show_inline),
        (playback_start_and_pause_persists_progress, crate::player::tests::run_start_and_pause_persists_progress),
        (playback_progress_sync_reports_each_outcome, crate::player::tests::run_progress_sync_reports_each_outcome),
        (playback_listening_sessions_continue_across_pauses_and_send_once_per_pause, crate::player::tests::run_listening_sessions_continue_across_pauses_and_send_once_per_pause),
        (playback_listening_sessions_kept_offline_go_in_one_request, crate::player::tests::run_listening_sessions_kept_offline_go_in_one_request),
        (playback_listening_sessions_go_along_with_a_finished_burst, crate::player::tests::run_listening_sessions_go_along_with_a_finished_burst),
        (playback_start_with_no_working_audio_engine_still_opens_with_an_error, crate::player::tests::run_start_with_no_working_audio_engine_still_opens_with_an_error),
        (playback_start_with_an_expired_session_and_nothing_cached_shows_a_login_error, crate::player::tests::run_start_with_an_expired_session_and_nothing_cached_shows_a_login_error),
        (playback_downloaded_track_is_preferred_over_streaming, crate::player::tests::run_downloaded_track_is_preferred_over_streaming),
        (playback_untrustworthy_complete_row_falls_back_to_streaming, crate::player::tests::run_untrustworthy_complete_row_falls_back_to_streaming),
        (playback_multi_track_mixed_downloaded_and_streamed, crate::player::tests::run_multi_track_mixed_downloaded_and_streamed),
        (playback_start_resumes_from_existing_progress, crate::player::tests::run_start_resumes_from_existing_progress),
        (playback_track_duration_mismatch_leaves_the_server_timeline_alone, crate::player::tests::run_track_duration_mismatch_leaves_the_server_timeline_alone),
        (playback_the_position_never_runs_past_its_file_in_the_server_timeline, crate::player::tests::run_the_position_never_runs_past_its_file_in_the_server_timeline),
        (playback_multi_track_advances_to_the_next_track, crate::player::tests::run_multi_track_advances_to_the_next_track),
        (playback_multi_track_final_track_marks_finished, crate::player::tests::run_multi_track_final_track_marks_finished),
        (playback_fully_downloaded_item_plays_offline, crate::player::tests::run_fully_downloaded_item_plays_offline),
        (playback_partially_downloaded_item_plays_until_a_gap_offline, crate::player::tests::run_partially_downloaded_item_plays_until_a_gap_offline),
        (playback_seek_across_track_boundary_lands_in_the_next_file, crate::player::tests::run_seek_across_track_boundary_lands_in_the_next_file),
        (playback_stalled_seek_keeps_the_last_known_position, crate::player::tests::run_stalled_seek_keeps_the_last_known_position),
        (playback_error_then_retry_reloads_from_the_last_good_position, crate::player::tests::run_error_then_retry_reloads_from_the_last_good_position),
        (playback_a_pause_that_never_lands_is_recovered, crate::player::tests::run_a_pause_that_never_lands_is_recovered),
        (playback_mpris_play_pause_right_after_an_unplug_is_ignored, crate::player::tests::run_mpris_play_pause_right_after_an_unplug_is_ignored),
        (playback_a_resume_seek_that_does_not_land_is_reissued, crate::player::tests::run_a_resume_seek_that_does_not_land_is_reissued),
        (playback_a_failed_start_keeps_both_books_positions, crate::player::tests::run_a_failed_start_keeps_both_books_positions),
        (playback_an_unchanged_position_is_not_pushed_twice, crate::player::tests::run_an_unchanged_position_is_not_pushed_twice),
        (playback_resuming_after_a_long_pause_adopts_newer_server_progress, crate::player::tests::run_resuming_after_a_long_pause_adopts_newer_server_progress),
        (playback_a_seek_while_paused_is_saved, crate::player::tests::run_a_seek_while_paused_is_saved),
        (playback_shutdown_flushes_the_final_position, crate::player::tests::run_shutdown_flushes_the_final_position),
        (playback_a_stream_that_ends_early_is_not_the_end_of_the_book, crate::player::tests::run_a_stream_that_ends_early_is_not_the_end_of_the_book),
        (playback_finished_download_takes_over_at_the_next_seek, crate::player::tests::run_finished_download_takes_over_at_the_next_seek),
        (playback_resume_jumps_straight_to_the_second_track, crate::player::tests::run_resume_jumps_straight_to_the_second_track),
        (playback_end_of_stream_pauses_and_marks_finished, crate::player::tests::run_end_of_stream_pauses_and_marks_finished),
        (playback_start_shows_the_cached_cover_and_keeps_it_when_the_fetch_fails, crate::player::tests::run_start_shows_the_cached_cover_and_keeps_it_when_the_fetch_fails),
        (playback_start_replaces_the_cover_once_a_valid_new_one_is_downloaded, crate::player::tests::run_start_replaces_the_cover_once_a_valid_new_one_is_downloaded),
        (playback_mini_bar_reflects_playback_state, crate::player::tests::run_mini_bar_reflects_playback_state),
        (playback_mini_bar_shows_a_warning_glyph_on_playback_error, crate::player::tests::run_mini_bar_shows_a_warning_glyph_on_playback_error),
        (playback_starting_a_new_item_flushes_the_previous_items_progress, crate::player::tests::run_starting_a_new_item_flushes_the_previous_items_progress),
        (playback_sync_pending_progress_pushes_the_current_position_while_paused, crate::player::tests::run_sync_pending_progress_pushes_the_current_position_while_paused),
        (playback_sync_pending_progress_does_not_clobber_a_newer_server_value, crate::player::tests::run_sync_pending_progress_does_not_clobber_a_newer_server_value),
        (playback_sync_pending_progress_is_a_no_op_when_nothing_is_loaded, crate::player::tests::run_sync_pending_progress_is_a_no_op_when_nothing_is_loaded),
        (playback_switching_books_never_plays_the_old_book, crate::player::tests::run_switching_books_never_plays_the_old_book),
        (playback_a_pause_while_a_book_loads_holds, crate::player::tests::run_a_pause_while_a_book_loads_holds),
        (playback_the_latest_start_wins, crate::player::tests::run_the_latest_start_wins),
        (playback_controls_on_a_book_that_failed_to_start_do_not_crash, crate::player::tests::run_controls_on_a_book_that_failed_to_start_do_not_crash),
        (playback_an_error_during_a_track_load_is_not_overridden, crate::player::tests::run_an_error_during_a_track_load_is_not_overridden),
        (playback_changes_during_a_track_load_are_applied_by_it, crate::player::tests::run_changes_during_a_track_load_are_applied_by_it),
        (playback_speed_picked_before_preroll_applies_once_ready, crate::player::tests::run_speed_picked_before_preroll_applies_once_ready),
        (playback_speed_refused_by_a_ready_pipeline_shows_the_running_speed, crate::player::tests::run_speed_refused_by_a_ready_pipeline_shows_the_running_speed),
        (playback_a_load_after_a_pause_is_not_a_stuck_pause, crate::player::tests::run_a_load_after_a_pause_is_not_a_stuck_pause),
        (playback_resuming_adopts_newer_progress_already_pulled_locally, crate::player::tests::run_resuming_adopts_newer_progress_already_pulled_locally),
        (playback_starting_at_a_chapter, crate::player::tests::run_starting_at_a_chapter),
        (playback_a_seek_right_after_a_pause_is_not_a_stuck_pause, crate::player::tests::run_a_seek_right_after_a_pause_is_not_a_stuck_pause),
        (playback_a_scrubber_drag_seeks_once, crate::player::tests::run_a_scrubber_drag_seeks_once),
        (playback_starting_the_loaded_book_does_not_reload_it, crate::player::tests::run_starting_the_loaded_book_does_not_reload_it),
        (playback_a_finished_book_stays_finished, crate::player::tests::run_a_finished_book_stays_finished),
        (playback_a_stream_error_reloads_once_by_itself, crate::player::tests::run_a_stream_error_reloads_once_by_itself),
        (playback_resuming_a_stream_reloads_when_its_connection_is_gone, crate::player::tests::run_resuming_a_stream_reloads_when_its_connection_is_gone),
        (playback_a_seek_that_never_lands_reloads_once_then_stops, crate::player::tests::run_a_seek_that_never_lands_reloads_once_then_stops),
        (playback_an_early_end_of_the_last_file_does_not_finish_the_book, crate::player::tests::run_an_early_end_of_the_last_file_does_not_finish_the_book),
        (playback_an_end_of_stream_from_before_a_seek_is_dropped, crate::player::tests::run_an_end_of_stream_from_before_a_seek_is_dropped),
        (playback_switching_books_during_a_resume_check_still_saves_the_outgoing_position, crate::player::tests::run_switching_books_during_a_resume_check_still_saves_the_outgoing_position),
        (playback_a_hanging_push_does_not_delay_local_writes, crate::player::tests::run_a_hanging_push_does_not_delay_local_writes),
        (playback_a_speed_picked_while_paused_on_a_stream_is_applied_at_play, crate::player::tests::run_a_speed_picked_while_paused_on_a_stream_is_applied_at_play),
        (playback_a_retired_player_goes_silent_and_stays_that_way, crate::player::tests::run_a_retired_player_goes_silent_and_stays_that_way),
        (playback_progress_actions_during_a_start_apply_once_loaded, crate::player::tests::run_progress_actions_during_a_start_apply_once_loaded),
        (playback_listeners_of_discarded_screens_are_dropped, crate::player::tests::run_listeners_of_discarded_screens_are_dropped),
        (playback_a_bouncing_headphone_jack_does_not_resume, crate::player::tests::run_a_bouncing_headphone_jack_does_not_resume),
        (playback_a_downloaded_book_starts_without_waiting_for_a_dead_server, crate::player::tests::run_a_downloaded_book_starts_without_waiting_for_a_dead_server),
        (playback_a_downloaded_book_starts_without_waiting_for_a_token_refresh, crate::player::tests::run_a_downloaded_book_starts_without_waiting_for_a_token_refresh),
        (player_screen_tapping_the_cover_opens_the_full_size_original, crate::screens::player::tests::run_tapping_the_cover_opens_the_full_size_original),
        (player_screen_closing_the_cover_viewer_early_keeps_the_download, crate::screens::player::tests::run_closing_the_cover_viewer_early_keeps_the_download),
        (player_screen_the_cover_viewer_in_low_memory_mode_shows_the_small_copy, crate::screens::player::tests::run_the_cover_viewer_in_low_memory_mode_shows_the_small_copy),
        (player_screen_cover_viewer_screenshots, crate::screens::player::tests::run_cover_viewer_screenshots),
        (item_detail_the_cover_opens_the_viewer_offline, crate::screens::item_detail::tests::run_the_cover_opens_the_viewer_offline),
        (playback_offline_mode_starts_a_downloaded_book_without_contacting_the_server, crate::player::tests::run_offline_mode_starts_a_downloaded_book_without_contacting_the_server),
        (playback_a_book_whose_start_track_is_not_downloaded_still_waits_for_the_server, crate::player::tests::run_a_book_whose_start_track_is_not_downloaded_still_waits_for_the_server),
        (player_screen_renders, crate::screens::player::tests::run),
        (sideways_rows_keep_every_touch_gesture, crate::widgets::sideways_scroll::tests::run_rows_keep_every_touch_gesture),
        (player_screen_playback_error_shows_the_banner, crate::screens::player::tests::run_playback_error_shows_the_banner),
        (player_screen_keyboard_actions, crate::screens::player::tests::run_keyboard_actions),
        (player_screen_chapters_sheet_lists_and_seeks, crate::screens::player::tests::run_chapters_sheet_lists_and_seeks),
        (player_screen_chapters_sheet_opens_at_the_current_chapter, crate::screens::player::tests::run_chapters_sheet_opens_at_the_current_chapter),
        (player_screen_chapter_buttons_jump_between_chapters, crate::screens::player::tests::run_chapter_buttons_jump_between_chapters),
        (player_screen_chapter_buttons_hide_for_a_book_without_chapters, crate::screens::player::tests::run_chapter_buttons_hide_for_a_book_without_chapters),
        (player_screen_scrubber_tracks_the_current_chapter, crate::screens::player::tests::run_scrubber_tracks_the_current_chapter),
        (player_screen_download_button_starts_a_download_and_reflects_state, crate::screens::player::tests::run_download_button_starts_a_download_and_reflects_state),
        (player_screen_download_progress_strip_reveals_while_downloading, crate::screens::player::tests::run_download_progress_strip_reveals_while_downloading),

        (download_scope_menu_shows_a_loading_placeholder_until_chapters_are_ready, crate::widgets::download_scope_menu::tests::run_shows_a_loading_placeholder_until_chapters_are_ready),
        (download_scope_menu_stepper_defaults_clamps_and_steps, crate::widgets::download_scope_menu::tests::run_stepper_defaults_clamps_and_steps),

        (download_scope_menu_next_chapters_row_uses_the_stepper_count, crate::widgets::download_scope_menu::tests::run_next_chapters_row_uses_the_stepper_count),

        (download_scope_menu_rows_show_size_estimates_that_track_the_stepper, crate::widgets::download_scope_menu::tests::run_rows_show_size_estimates_that_track_the_stepper),

        (download_scope_menu_rows_block_when_free_space_is_insufficient, crate::widgets::download_scope_menu::tests::run_rows_block_when_free_space_is_insufficient),
        (download_scope_menu_a_failed_download_toasts_the_reason, crate::widgets::download_scope_menu::tests::run_a_failed_download_toasts_the_reason),

        (item_options_menu_buttons_invoke_their_callback_and_popdown, crate::widgets::item_options_menu::tests::run_buttons_invoke_their_callback_and_popdown),
        (player_screen_speed_popover_changes_playback_speed, crate::screens::player::tests::run_speed_popover_changes_playback_speed),
        (player_screen_sleep_timer_end_of_chapter_pauses_at_the_boundary, crate::screens::player::tests::run_sleep_timer_end_of_chapter_pauses_at_the_boundary),
        (player_screen_mark_as_finished_button_updates_progress, crate::screens::player::tests::run_mark_as_finished_button_updates_progress),
        (player_screen_reset_progress_button_resets_position_and_seeks, crate::screens::player::tests::run_reset_progress_button_resets_position_and_seeks),
        (cover_image_rendering, crate::widgets::cover_image::tests::run),
        (cover_image_decodes_at_the_requested_size_not_the_source_size, crate::widgets::cover_image::tests::run_decodes_at_the_requested_size_not_the_source_size),
        (cover_image_cache_hit_avoids_re_reading_disk, crate::widgets::cover_image::tests::run_cache_hit_avoids_re_reading_disk),
        (cover_image_low_memory_mode_turns_covers_off_and_back_on, crate::widgets::cover_image::tests::run_low_memory_mode_turns_covers_off_and_back_on),
        (low_memory_mode_state_notifies_and_persists, crate::low_memory_mode::tests::run_low_memory_mode_state_notifies_and_persists),
        (cover_image_a_stale_decode_does_not_clobber_a_newer_path, crate::widgets::cover_image::tests::run_a_stale_decode_does_not_clobber_a_newer_path),
        (cover_image_lru_eviction_keeps_recently_accessed_entries, crate::widgets::cover_image::tests::run_lru_eviction_keeps_recently_accessed_entries),
        (debouncer_fires_again_after_a_previous_debounce_already_fired, crate::widgets::tests::run_fires_again_after_a_previous_debounce_already_fired),
        (debouncer_only_the_last_schedule_within_the_delay_runs, crate::widgets::tests::run_only_the_last_schedule_within_the_delay_runs),
        (downloads_engine_start_fetches_only_the_needed_track_and_reaches_complete, crate::downloads::tests::run_start_download_fetches_only_the_needed_track_and_reaches_complete),
        (downloads_engine_cancel_item_stops_the_track_from_reaching_complete, crate::downloads::tests::run_cancel_item_stops_the_track_from_reaching_complete),
        (downloads_engine_wifi_only_blocks_a_download_on_a_metered_connection, crate::downloads::tests::run_wifi_only_blocks_a_download_on_a_metered_connection),
        (downloads_engine_clear_item_deletes_files_and_publishes_idle, crate::downloads::tests::run_clear_item_deletes_files_and_publishes_idle),
        (downloads_engine_start_with_no_chapters_fetches_every_track, crate::downloads::tests::run_start_download_with_no_chapters_fetches_every_track),
        (downloads_engine_set_wifi_only_only_affects_downloads_started_afterwards, crate::downloads::tests::run_set_wifi_only_only_affects_downloads_started_afterwards),
        (downloads_screen_empty_state_renders_when_nothing_is_downloaded, crate::screens::downloads::tests::run_empty_state_renders_when_nothing_is_downloaded),
        (downloads_screen_in_progress_download_shows_a_cancel_row, crate::screens::downloads::tests::run_in_progress_download_shows_a_cancel_row),
        (downloads_screen_in_progress_download_shows_progress_and_speed, crate::screens::downloads::tests::run_in_progress_download_shows_progress_and_speed),
        (downloads_screen_stop_keeps_completed_chapters_and_stops_the_job, crate::screens::downloads::tests::run_stop_keeps_completed_chapters_and_stops_the_job),
        (downloads_screen_completed_download_can_be_removed, crate::screens::downloads::tests::run_completed_download_can_be_removed),
        (downloads_screen_database_failures_are_reported, crate::screens::downloads::tests::run_database_failures_are_reported),
        (downloads_screen_tapping_a_row_opens_it_and_remove_does_not, crate::screens::downloads::tests::run_tapping_a_row_opens_it_and_remove_does_not),
        (downloads_screen_clear_all_button_is_disabled_until_something_is_downloaded, crate::screens::downloads::tests::run_clear_all_button_is_disabled_until_something_is_downloaded),
        (downloads_screen_clear_all_cancelled_leaves_downloads_intact, crate::screens::downloads::tests::run_clear_all_cancelled_leaves_downloads_intact),
        (downloads_screen_clear_all_confirmed_removes_every_download_and_its_files, crate::screens::downloads::tests::run_clear_all_confirmed_removes_every_download_and_its_files),
    }

    /// The only `#[test]` a plain `cargo test` runs in this binary: re-executes this same binary
    /// once per scenario so each gets its own GTK process — one flaky scenario can no longer fail
    /// the whole suite, and the failing scenario's name is its own isolation command.
    #[test]
    fn gtk_fast_scenarios() {
        let exe = std::env::current_exe().expect("locate this test binary");
        let mut failed = Vec::new();
        for name in GTK_SCENARIOS {
            // The child inherits our stdio, so its per-scenario output (panics, libtest's result
            // line) lands inline right here in the parent's log.
            let status = std::process::Command::new(&exe)
                .arg(format!("tests::{name}"))
                .args(["--exact", "--ignored"])
                .status()
                .expect("re-exec this test binary for the scenario process");
            if !status.success() {
                failed.push(*name);
            }
        }
        assert!(
            failed.is_empty(),
            "{}/{} scenarios failed: {:?}\nre-run one in isolation with: cargo test -p audiobooklet -- --exact tests::<name> --ignored",
            failed.len(),
            GTK_SCENARIOS.len(),
            failed,
        );
    }

    #[test]
    #[ignore]
    fn gtk_live_demo_server_scenarios() {
        gtk4::init().expect("gtk4::init for this test");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();

        crate::screens::welcome::tests::run_live(&runtime);
        crate::screens::home::tests::run_live(&runtime);
        crate::screens::library::tests::run_live(&runtime);
    }
}
