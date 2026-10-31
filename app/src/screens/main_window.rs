//! The post-login shell: a 4-destination bottom tab bar (Home / Library / Downloads / Settings),
//! per `docs/design/ui-spec.md` §2's phone-width navigation model. Built from `AdwViewStack` +
//! `AdwViewSwitcherBar` — both available at this crate's libadwaita `v1_2` ceiling (confirmed by
//! reading `libadwaita-0.7.2/src/auto/mod.rs`'s cfg gates: neither is behind a version feature
//! gate, unlike `AdwToolbarView`/`AdwBreakpoint`/`AdwNavigationView`/`AdwNavigationSplitView`,
//! which are all `v1_4` and out of reach here) — so, unlike the mini-player bar or the Welcome
//! screen's auth-mode toggle, this needs no hand-rolled compatibility widget.
//!
//! Only Home has real content so far; Library/Downloads/Settings are `AdwStatusPage` stubs, each
//! still a real page in the stack so wiring in an actual screen later is a one-line swap. No
//! wide-screen sidebar layout (`AdwNavigationSplitView`/`AdwBreakpoint`, both v1.4+) — phone-width
//! single-pane only, left as a documented follow-up.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use abs_player::call_watch::CallWatcher;
use abs_player::connectivity_watch::ConnectivityWatcher;
use abs_player::route_watch::RouteWatcher;
use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use abs_core::settings::{PlaybackSettings, Theme};
use abs_storage::models::{Account, Server};
use abs_storage::AppPaths;

use crate::i18n::{tr, tr_args, tr_noop};
use crate::player::{self, PlayRequest};
use crate::screens;

/// Holds a GTK application-level suspend inhibitor for exactly as long as something is playing —
/// see `docs/design/ui-spec.md`'s "Hardware controls & interruptions" and this plan's Librem 5
/// field report (playback stopped mid-book because the phone suspended). `Application::inhibit`
/// asks the session manager (`org.gnome.SessionManager` under GNOME/Phosh) to keep the SoC out
/// of suspend; inside Flatpak, GTK routes this through the `org.freedesktop.portal.Inhibit`
/// portal automatically, so no extra manifest permission is needed. Deliberately `SUSPEND` only,
/// never `IDLE`: the screen should still blank and lock during playback, only the device must
/// stay awake to keep decoding and streaming audio.
///
/// `cookie: 0` is GTK's own sentinel for "no inhibitor held" (returned by a real `inhibit()`
/// call, and never a valid cookie itself) — reused here as the initial/cleared state rather than
/// wrapping it in an `Option`, so a session manager that's unreachable at inhibit time is
/// silently treated the same as "not currently playing" instead of a special case.
/// A suspend inhibit that takes longer than this to be answered gets a warning in the log.
const SLOW_SESSION_MANAGER_CALL: std::time::Duration = std::time::Duration::from_millis(100);

struct SuspendInhibitGuard {
    app: Option<gtk4::Application>,
    window: adw::ApplicationWindow,
    cookie: std::cell::Cell<u32>,
}

impl SuspendInhibitGuard {
    /// Called on every published snapshot (so up to 4×/s while playing) — cheap by construction:
    /// `inhibit`/`uninhibit` are only actually asked for on the specific tick `is_playing` flips,
    /// never on every unchanged snapshot in between.
    fn update(&self, is_playing: bool) {
        let Some(app) = &self.app else { return };
        // Both calls are synchronous D-Bus round trips to the session manager, made on the main
        // loop: a slow one is a frozen UI (and a late pause/play response), so say so.
        let started = std::time::Instant::now();
        match (is_playing, self.cookie.get()) {
            (true, 0) => {
                let cookie = app.inhibit(Some(&self.window), gtk4::ApplicationInhibitFlags::SUSPEND, Some(&tr("Playing an audiobook")));
                self.cookie.set(cookie);
            }
            (false, cookie) if cookie != 0 => {
                app.uninhibit(cookie);
                self.cookie.set(0);
            }
            _ => return,
        }
        if started.elapsed() > SLOW_SESSION_MANAGER_CALL {
            tracing::warn!(elapsed_ms = started.elapsed().as_millis() as u64, is_playing, "the session manager was slow to answer the suspend inhibit; the UI waited for it");
        }
    }
}

impl Drop for SuspendInhibitGuard {
    /// Releases a still-held inhibitor when this guard itself goes away (signing out, switching
    /// accounts, or the app quitting mid-playback all drop the controller — and with it every
    /// listener, this one included) — otherwise a suspend block could outlive the very playback
    /// it was justified by.
    fn drop(&mut self) {
        let cookie = self.cookie.get();
        if cookie != 0 {
            if let Some(app) = &self.app {
                app.uninhibit(cookie);
            }
        }
    }
}

/// Pushes every book's progress the server hasn't confirmed yet (listened to offline, or whose
/// push failed), not just the one loaded in the player — see
/// `abs_core::progress_sync::reconcile_all_progress`. Best-effort: a failure leaves the rows
/// marked for the next reconnect or sync.
fn push_unconfirmed_progress(pool: sqlx::SqlitePool, session: abs_core::auth::Session) {
    glib::spawn_future_local(async move {
        let connection = match session.connection_target().await {
            Ok(connection) => connection,
            Err(err) => {
                tracing::info!(%err, "couldn't load connection settings; unconfirmed progress stays local for now");
                return;
            }
        };
        let access_token = session.access_token().await;
        if let Err(err) =
            abs_core::progress_sync::reconcile_all_progress(
                &pool,
                &connection,
                &access_token,
                session.account_id(),
                session.server_id(),
                crate::sync_coordinator::loaded_item(session.server_id(), session.account_id()).as_deref(),
            )
            .await
        {
            tracing::info!(%err, "couldn't push unconfirmed progress on reconnect; will retry on the next one");
        }
    });
}

fn position_adopted_toast_title(to_seconds: f64) -> String {
    // TRANSLATORS: {time} is a playback position such as "1:23:45".
    tr_args("Continued at {time} from another device", &[("time", &screens::player::format_hms(to_seconds))])
}

pub struct MainWindow {
    pub root: gtk4::Widget,
    /// Kept alive for the app's whole lifetime — dropping it unsubscribes from ModemManager's
    /// D-Bus signals. `None` when no system bus (or no ModemManager on it) was reachable; call
    /// interruption (pause on ringing, resume after an unanswered call — see
    /// `PlayerController::handle_call_event`) is simply unavailable in that case, never a fatal
    /// error.
    _call_watcher: Option<abs_player::call_watch::ModemManagerCallWatcher>,
    /// Same lifetime contract as `_call_watcher`: retained forever, `None` when no audio server
    /// (PulseAudio/PipeWire) was reachable — headphone unplug/replug reaction is then simply
    /// unavailable, never a fatal error.
    _route_watcher: Option<abs_player::route_watch::PulseRouteWatcher>,
    /// Same lifetime contract as `_call_watcher`: retained forever, `None` when no system bus (or
    /// no NetworkManager on it) was reachable — reconnect-triggered progress sync is then simply
    /// unavailable, never a fatal error.
    _connectivity_watcher: Option<abs_player::connectivity_watch::NetworkManagerConnectivityWatcher>,
    /// This shell's handle on the app-wide download manager (`downloads::app_wide`), scoped to
    /// its server. Every screen that needs it (Downloads, Settings, Item Detail, Player) is handed
    /// its own clone at build time; this field itself is never read back (hence the allow).
    #[allow(dead_code)]
    pub download_manager: crate::downloads::DownloadManager,
    /// Retired when this shell is dropped (replaced by the next account's, or by the login
    /// screen) — see `PlayerController::retire`. Without it the old player kept playing, and kept
    /// answering the media keys, behind the new shell.
    controller: player::PlayerController,
    /// Dropped with this shell: the download listeners its screens registered (through
    /// `download_manager`, scoped to it) go with it, while the downloads themselves carry on.
    _shell_alive: Rc<()>,
    #[cfg(test)]
    hooks: TestHooks,
}

impl Drop for MainWindow {
    fn drop(&mut self) {
        self.controller.retire();
    }
}

#[cfg(test)]
pub struct TestHooks {
    pub stack: adw::ViewStack,
    pub switcher_bar: adw::ViewSwitcherBar,
    pub mini_bar: crate::player::MiniPlayerHooks,
    /// Invokes the exact same navigation the mini bar's tap/swipe-up gesture triggers (MP-5/
    /// MP-6), without needing to synthesize a real `GestureDrag` sequence — there's no existing
    /// raw-gesture test harness in this codebase, so tests assert on the outcome of this shared
    /// closure instead.
    pub open_player: Rc<dyn Fn()>,
    /// The shell's keyboard actions (ui-spec §6), so tests can activate them directly — the
    /// accel-to-action mapping itself is GTK-level and needs real key events to exercise.
    pub switch_tab: gtk4::gio::SimpleAction,
    pub play_pause: gtk4::gio::SimpleAction,
    pub open_library_search: gtk4::gio::SimpleAction,
    /// The Library tab's search entry, the focus target of `open-library-search`.
    pub library_search: gtk4::SearchEntry,
    /// Settings → Playback's headphone-behavior switches, so tests can assert initial state and
    /// activation (the row-tap path) without reaching into the screen's internals.
    pub pause_on_unplug_switch: gtk4::Switch,
    pub resume_on_replug_switch: gtk4::Switch,
    /// Hosts the one-shot playback-error toast (see `build`'s `last_toasted_error` listener) —
    /// shared by every tab, unlike each screen's own per-screen `AdwToastOverlay`.
    pub toast_overlay: adw::ToastOverlay,
}

#[cfg(test)]
impl MainWindow {
    pub fn test_hooks(&self) -> &TestHooks {
        &self.hooks
    }
}

/// Below this, a drag is treated as a tap (open); at or above `SWIPE_UP_MIN_DISTANCE_PX` of
/// predominantly-upward movement, it's treated as a swipe-up (also open) — see
/// `mini_bar_gesture_should_open`'s doc comment. First-pass calibration constants only: the
/// ui-spec flags the mini bar's swipe-up hit-zone as not yet validated against phosh's own
/// bottom-edge shell gestures on real Librem 5 hardware, so these are expected to be re-tuned
/// once that on-device measurement happens.
const TAP_MAX_MOVEMENT_PX: f64 = 8.0;
const SWIPE_UP_MIN_DISTANCE_PX: f64 = 24.0;

/// Whether a completed drag on the mini bar should open the full player — true for a tap
/// (movement in every direction stayed within `TAP_MAX_MOVEMENT_PX`, MP-5's guaranteed fallback)
/// or a predominantly-upward swipe that traveled at least `SWIPE_UP_MIN_DISTANCE_PX` (MP-6).
/// `offset_x`/`offset_y` are `GestureDrag::offset()`'s values: the total displacement from press
/// to release, with negative `offset_y` meaning "moved up." A drag that's mostly horizontal, or
/// swipes down, does nothing — same as a `GestureClick` simply not recognizing anything but its
/// own click. `pub(crate)` so `screens::item_detail`'s own mini bar reuses the identical decision
/// rather than a second copy.
pub(crate) fn mini_bar_gesture_should_open(offset_x: f64, offset_y: f64) -> bool {
    let is_tap = offset_x.abs() <= TAP_MAX_MOVEMENT_PX && offset_y.abs() <= TAP_MAX_MOVEMENT_PX;
    let is_swipe_up = offset_y <= -SWIPE_UP_MIN_DISTANCE_PX && offset_y.abs() > offset_x.abs();
    is_tap || is_swipe_up
}

/// A signed-in shell's construction from everything it needs. The argument count is the shell's
/// genuine composition — pool, paths, the session it fronts, the two long-lived managers, the
/// screen data and the window itself — not an accident to refactor into a params struct; the
/// allow documents that judgment call.
#[allow(clippy::too_many_arguments)]
pub fn build(
    pool: SqlitePool,
    paths: AppPaths,
    server: Server,
    account: Account,
    session: abs_core::auth::Session,
    playback_settings: PlaybackSettings,
    theme: Theme,
    servers_with_accounts: Vec<(Server, Vec<Account>)>,
    window: adw::ApplicationWindow,
) -> MainWindow {
    let _slow = crate::perf::SlowJob::new("main window build");
    let mut steps = crate::perf::Steps::new("main window build steps", "startup");
    steps.step("backend_and_mini_bar");
    let mini_bar = player::build_mini_bar(pool.clone(), paths.clone(), player::real_backend());
    steps.step("mpris");

    // MPRIS registration is best-effort — no session bus (a bare console, a locked-down sandbox)
    // must never be fatal to playback, so a failure here is just a warning. On success, the
    // bridge becomes a permanent snapshot listener (via the foundation refactor's `add_listener`)
    // so the lock-screen/Shell media card stays current for the app's whole lifetime.
    //
    // "audiobooklet" (not "Audiobookshelf") — `mpris::register` builds the bus name as
    // `org.mpris.MediaPlayer2.{app_name}`, and the Flatpak manifest's `finish-args` only grants
    // `--own-name=org.mpris.MediaPlayer2.audiobooklet`. Under Flatpak, `--own-name` is an exact-match
    // allowlist, so this previously asked to own a name the sandbox never actually permitted —
    // silently non-fatal (MPRIS failures are always treated as best-effort, per the comment
    // above), but MPRIS would never actually have worked inside the Flatpak build.
    let mpris_bridge: Rc<dyn abs_player::mpris::MprisCommands> = Rc::new(player::MprisBridge::new(mini_bar.controller.clone()));
    match abs_player::mpris::register("audiobooklet", crate::application::APP_NAME, mpris_bridge) {
        Ok(mpris_handle) => {
            mini_bar.controller.add_listener(move |snapshot| mpris_handle.update(player::mpris_state_from_snapshot(snapshot)));
        }
        Err(err) => tracing::warn!(%err, "couldn't register MPRIS media player; system media integration will be unavailable"),
    }

    // Keeps the device out of suspend for exactly as long as something is playing — see
    // `SuspendInhibitGuard`'s own doc comment. `window.application()` is `None` only in a test
    // that builds a bare `adw::ApplicationWindow` with no `adw::Application` behind it, in which
    // case this listener is a permanent, harmless no-op.
    let suspend_inhibit = SuspendInhibitGuard { app: window.application(), window: window.clone(), cookie: std::cell::Cell::new(0) };
    mini_bar.controller.add_listener(move |snapshot| suspend_inhibit.update(snapshot.is_playing));

    // One toast per *new* playback failure (never a repeat of the same ongoing one — every
    // snapshot while the error persists would otherwise re-fire this on every 250ms tick) — the
    // shell-wide heads-up for whichever tab is open when it happens; the mini bar's own warning
    // glyph is the persistent indicator, and the Full Player screen's banner is where the honest
    // detail lives. `open_player` (built below) is threaded in once it exists.
    let last_toasted_error: Rc<RefCell<Option<abs_player::PlaybackErrorKind>>> = Rc::new(RefCell::new(None));

    steps.step("call_watcher");
    // Phone-call interruption is best-effort in the same way: no system bus, or no ModemManager
    // on it, must never be fatal — it just means this feature is unavailable. The callback is
    // one line by design, like the route watcher's below: all the semantics (pause on ringing;
    // resume only after an unanswered incoming call that was itself the reason for the pause)
    // live in `handle_call_event`, where the tests can reach them. The watcher itself is
    // retained on `MainWindow` (see its field doc) — dropping it would unsubscribe immediately.
    let call_watcher = match abs_player::call_watch::ModemManagerCallWatcher::new() {
        Ok(mut watcher) => {
            let controller = mini_bar.controller.clone();
            watcher.start(Box::new(move |event| controller.handle_call_event(event)));
            Some(watcher)
        }
        Err(err) => {
            tracing::warn!(%err, "couldn't watch ModemManager for call interruptions; this feature will be unavailable");
            None
        }
    };

    // Headphone unplug/replug (see `route_watch`'s module docs) — best-effort in the same way:
    // no audio server to talk to means the feature is unavailable, never fatal. The behavior
    // knobs come from Settings → Playback and are applied before the first event can arrive.
    // The callback is one line by design: all the semantics (only unplug pauses; a replug only
    // lifts an unplug pause, never a manual one or a call) live in `handle_route_event`, where
    // the tests can reach them.
    mini_bar.controller.set_headphone_behavior(
        playback_settings.pause_on_headphone_unplug,
        playback_settings.resume_on_headphone_replug,
    );
    // Same story for the playback config: the start-of-session speed and skip intervals live on
    // the controller, and the consumers read them at call time — a Settings edit reaches even
    // MPRIS's next/previous without rebuilding any screen.
    mini_bar.controller.set_playback_config(
        playback_settings.default_speed,
        playback_settings.skip_back_seconds as f64,
        playback_settings.skip_forward_seconds as f64,
    );
    // Same story again for burst buffering — applied before any track can load, so the very
    // first playback of this session already honors Settings → Playback's switch rather than
    // needing a Settings edit (which re-applies it) to take effect.
    mini_bar.controller.set_burst_buffering(playback_settings.burst_buffering);
    steps.step("route_watcher");
    let route_watcher = match abs_player::route_watch::PulseRouteWatcher::new() {
        Ok(mut watcher) => {
            let controller = mini_bar.controller.clone();
            watcher.start(Box::new(move |event| controller.handle_route_event(event)));
            Some(watcher)
        }
        Err(err) => {
            tracing::warn!(%err, "couldn't watch the audio server for headphone changes; unplug-pause will be unavailable");
            None
        }
    };

    // Reconnect-triggered progress sync — best-effort in the same way as call-watching/route-
    // watching above: no system bus (or no NetworkManager on it) must never be fatal, it just
    // means a device that goes offline while paused/idle won't push its pending local position
    // until the user does something else that happens to trigger a sync.
    steps.step("network_watchers");
    let connectivity_watcher = match abs_player::connectivity_watch::NetworkManagerConnectivityWatcher::new() {
        Ok(mut watcher) => {
            let controller = mini_bar.controller.clone();
            let pool = pool.clone();
            let session = session.clone();
            watcher.start(Box::new(move || {
                // Offline mode ignores the network coming back; switching it off catches up.
                if session.is_offline() {
                    return;
                }
                controller.sync_pending_progress();
                push_unconfirmed_progress(pool.clone(), session.clone());
                controller.network_back(&session);
            }));
            Some(watcher)
        }
        Err(err) => {
            tracing::warn!(%err, "couldn't watch NetworkManager for connectivity changes; reconnect-triggered progress sync will be unavailable");
            None
        }
    };

    // Wi-Fi-only detection is best-effort in the same way as MPRIS/call-watching above: no system
    // bus (or no NetworkManager on it) must never be fatal — it just means metered-connection
    // detection is unavailable, and `UnknownNetworkMonitor` makes that read as "undeterminable"
    // rather than silently guessing "not metered".
    // Shared by every shell, so a download keeps going across a server switch; this shell's
    // handle hears only about its own server, and its listeners go with the shell.
    let shell_alive = Rc::new(());
    let download_manager = crate::downloads::app_wide(|| {
        let network_monitor: Box<dyn abs_player::network_watch::NetworkMonitor> = match abs_player::network_watch::NetworkManagerMonitor::new() {
            Ok(monitor) => Box::new(monitor),
            Err(err) => {
                tracing::warn!(%err, "couldn't watch NetworkManager for metered-connection detection; Wi-Fi-only downloads will be unavailable");
                Box::new(abs_player::network_watch::UnknownNetworkMonitor)
            }
        };
        crate::downloads::DownloadManager::new(pool.clone(), paths.clone(), network_monitor, playback_settings.wifi_only_downloads)
    })
    .scoped_to(&server.id, &shell_alive);
    // Shared between Home and Library only (per docs/design/ui-spec.md, "not a per-screen
    // setting") — see `crate::offline_mode::OfflineModeState`'s doc for why this must be a single
    // shared instance rather than each screen loading its own copy.
    let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
    // "Low memory mode": the covers follow it (no decoding, a small texture cache) and so do the
    // lists that show them (Home, Library — hooked up where they are built, below).
    let low_memory_mode = crate::low_memory_mode::LowMemoryModeState::new(pool.clone());
    low_memory_mode.add_listener(crate::widgets::cover_image::set_low_memory_mode);
    // Offline mode cuts every server call made through the session (see
    // `abs_core::auth::Session::set_offline`); switching it off catches up on what was kept
    // locally meanwhile — this book's progress and any other book's unpushed progress.
    offline_mode.add_listener({
        let controller = mini_bar.controller.clone();
        let session = session.clone();
        let pool = pool.clone();
        move |on| {
            if session.is_offline() == on {
                return;
            }
            session.set_offline(on);
            if on {
                tracing::info!("offline mode on: network disabled");
            } else {
                tracing::info!("offline mode off: catching up with the server");
                controller.sync_pending_progress();
                push_unconfirmed_progress(pool.clone(), session.clone());
                controller.network_back(&session);
            }
        }
    });

    // Listening sessions: tidy up what a previous run left (open sessions closed, long-sent ones
    // pruned), send what's waiting, and send again whenever a library sync has just woken the
    // radio anyway.
    {
        let controller = mini_bar.controller.clone();
        let session = session.clone();
        let pool = pool.clone();
        glib::spawn_future_local(async move {
            if let Err(err) = abs_core::listening_sessions::tidy_up(&pool).await {
                tracing::warn!(%err, "couldn't tidy up the stored listening sessions");
            }
            controller.send_listening_sessions(&session, crate::listening::SendReason::Due);
        });
    }
    crate::sync_coordinator::on_network_used(&server.id, session.account_id(), {
        let controller = mini_bar.controller.downgrade();
        let session = session.clone();
        move || {
            let Some(controller) = controller.upgrade() else { return false };
            controller.send_listening_sessions(&session, crate::listening::SendReason::Opportunistic);
            true
        }
    });

    steps.step("tabs_and_rest");
    let stack = adw::ViewStack::new();

    let shell_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    // Wraps the shell so a playback-error toast (see `last_toasted_error` above) can show
    // regardless of which tab is open — every screen already has its own per-screen
    // `AdwToastOverlay` for its own toasts, but nothing wrapped the shell itself before this.
    // `root` (this overlay) is still what gets swapped in/out as the window's content — every
    // existing `swap_content`/`MainWindow.root` use below is unchanged, just backed by a
    // different widget now.
    let root = adw::ToastOverlay::new();
    root.set_child(Some(&shell_box));
    // Reported here rather than on Home's or Library's own overlay: this overlay is on screen
    // whichever of the two tabs the toggle was flipped from.
    offline_mode.set_on_persist_error({
        let root = root.clone();
        move |err| crate::error_reporting::report_background_error(&root, &tr("Saving offline mode"), err)
    });
    low_memory_mode.set_on_persist_error({
        let root = root.clone();
        move |err| crate::error_reporting::report_background_error(&root, &tr("Saving low memory mode"), err)
    });

    // Opens the full player by swapping the window's content — there's no
    // `AdwNavigationView`/`AdwDialog` available at this crate's libadwaita ceiling (both v1.4+),
    // so this is the same content-swap mechanism `application.rs` already uses for Welcome -> main
    // window. Collapsing restores `root` (this shell), not a fresh `build()` call — no state lost.
    //
    // The player screen's keyboard actions ride along: its `SimpleActionGroup` is merged under
    // the "player" prefix for exactly as long as the screen is open, and removed on collapse, so
    // the arrow-key/speed/`c`/`t`/Escape accelerators (ui-spec §6) are inert everywhere else.
    //
    // Shared by the mini bar's tap and swipe-up gestures below (ui-spec's "Player — mini",
    // MP-5/MP-6) *and* by `start_playback` below (Item Detail's Play/Resume/chapter-tap path), so
    // every trigger can never drift into opening the player differently. Defined here, ahead of
    // `start_playback`, specifically so it can capture this too. `screens::player::build` requires
    // a loaded book (see its own doc comment); `open_player` below waits for one, so the mini bar
    // can be tapped while a book is still starting.
    let open_player_now: Rc<dyn Fn()> = Rc::new({
        let controller = mini_bar.controller.clone();
        let window = window.clone();
        let root = root.clone();
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let stack = stack.clone();
        move || {
            let player_screen = screens::player::build(
                pool.clone(),
                controller.clone(),
                download_manager.clone(),
                {
                    let window = window.clone();
                    let root = root.clone();
                    move || {
                        crate::widgets::swap_content(&window, &root);
                        window.insert_action_group("player", None::<&gtk4::gio::ActionGroup>);
                    }
                },
                {
                    let window = window.clone();
                    let root = root.clone();
                    let stack = stack.clone();
                    move || {
                        crate::widgets::swap_content(&window, &root);
                        window.insert_action_group("player", None::<&gtk4::gio::ActionGroup>);
                        stack.set_visible_child_name("downloads");
                    }
                },
            );
            window.insert_action_group("player", Some(player_screen.actions.upcast_ref::<gtk4::gio::ActionGroup>()));
            crate::widgets::swap_content(&window, &player_screen.root);
        }
    });
    // Opens the player for the book that is loaded or starting — the Player is built while a
    // start is still resolving (it shows the loading state and picks the book up as it loads),
    // rather than waiting for the load (a start over a slow connection used to never open it).
    // Nothing loaded and nothing starting: there is nothing to show.
    let open_player: Rc<dyn Fn()> = Rc::new({
        let controller = mini_bar.controller.clone();
        move || {
            if controller.current_download_context().is_some() {
                open_player_now();
            }
        }
    });

    mini_bar.controller.add_listener({
        let toast_overlay = root.clone();
        let open_player = open_player.clone();
        move |snapshot| {
            let new_kind = snapshot.last_error.as_ref().map(|err| err.kind);
            let mut last = last_toasted_error.borrow_mut();
            if new_kind.is_some() && new_kind != *last {
                if let Some(kind) = new_kind {
                    let toast = adw::Toast::new(&tr(screens::player::short_message(kind)));
                    toast.set_button_label(Some(&tr("View")));
                    toast.connect_button_clicked({
                        let open_player = open_player.clone();
                        move |_| open_player()
                    });
                    toast_overlay.add_toast(toast);
                }
            }
            *last = new_kind;
        }
    });

    // One toast per progress-sync failure *episode*, same rate-limiting idea as the playback
    // error toast above: a sync is attempted every few minutes while playing, and a dead server
    // must not toast on every attempt. A successful sync ends the episode; a failure of a
    // different kind (session expired after a network outage) is news, so it toasts too.
    mini_bar.controller.set_on_progress_sync({
        let toast_overlay = root.clone();
        let last_failure: Rc<Cell<Option<player::ProgressSyncOutcome>>> = Rc::new(Cell::new(None));
        move |outcome| {
            if let Some(title) = progress_sync_toast(&last_failure, outcome) {
                toast_overlay.add_toast(adw::Toast::new(&tr(title)));
            }
        }
    });

    // Quitting (closing the window, Ctrl+Q) saves the final position first. Weak, so a shell
    // replaced by a sign-out or account switch doesn't stay alive through this handler.
    if let Some(app) = window.application() {
        let controller = mini_bar.controller.downgrade();
        app.connect_shutdown(move |_| {
            if let Some(controller) = controller.upgrade() {
                controller.flush_on_shutdown();
            }
        });
    }

    // Resuming after a long pause jumped to newer progress from another device — say so, and let
    // the listener put it back if this device's position was the one they wanted.
    mini_bar.controller.set_on_position_adopted({
        let toast_overlay = root.clone();
        let controller = mini_bar.controller.clone();
        move |from, to| {
            let toast = adw::Toast::builder()
                .title(position_adopted_toast_title(to))
                .button_label(tr("Undo"))
                .timeout(10)
                .build();
            let controller = controller.clone();
            // The Undo belongs to the book it was offered for; another book may be loaded by
            // the time it's tapped.
            let item_id = controller.current_item_id();
            toast.connect_button_clicked(move |_| {
                if item_id.is_none() || controller.current_item_id() != item_id {
                    tracing::info!("undo ignored: a different book is loaded now");
                    return;
                }
                controller.seek_to_seconds(from);
                controller.save_progress_now();
            });
            toast_overlay.add_toast(toast);
        }
    });

    // Starts real playback (`PlayerController::start`) for a request, optionally seeking to a
    // chapter once it's ready, then opens the full Player screen once playback has actually
    // started — the one place in the shell that actually calls into `abs-player`/
    // `abs-core::streaming` on a card's behalf. Used below as Item Detail's own `on_play` (a
    // chapter tap or Play/Resume) — Item Detail itself never touches the controller directly,
    // same "screens report intent, the shell acts on it" boundary `home.rs`/`library.rs` already
    // draw for `on_open`.
    let start_playback = {
        let controller = mini_bar.controller.clone();
        let session = session.clone();
        let open_player = open_player.clone();
        move |request: PlayRequest, start_chapter: Option<usize>| {
            // The default speed is read at call time, not captured — a "Default speed" change in
            // Settings applies to the next playback without rebuilding the shell.
            // A tapped chapter is part of the start itself — it used to be a seek applied once
            // `chapters()` was non-empty, which the *previous* book's chapters satisfied at once.
            controller.start_with(session.clone(), request, controller.default_speed(), start_chapter);
            // `start_with()` resolves asynchronously (network + stream resolution); `open_player`
            // opens the Player once the book has loaded (or failed — the Player then shows why).
            open_player();
        }
    };

    // "Log in again" (Home/Library's authorization-failure states) hands control back to the
    // shell: the window's content is swapped for the login screen, pre-filled with this session's
    // URL and username so only the password is left to type. The seed describes the session this
    // shell was built for — exactly what a re-login compares against.
    let on_relogin = {
        let window = window.clone();
        let pool = pool.clone();
        let paths = paths.clone();
        // Cloned into locals first: a `move` closure would otherwise capture the `server`/
        // `account` fields it touches by value (editions' disjoint field capture), pulling the
        // rug out from under the `server.clone()`/`account.clone()` calls further down.
        let seed_server_id = server.id.clone();
        let seed_server_url = server.url.clone();
        let seed_account_id = account.id.clone();
        let seed_username = account.username.clone();
        move || {
            let seed = abs_core::accounts::ReloginSeed {
                server_id: seed_server_id.clone(),
                account_id: seed_account_id.clone(),
                url: seed_server_url.clone(),
                username: seed_username.clone(),
            };
            crate::application::show_welcome(&window, pool.clone(), paths.clone(), playback_settings, Some(seed));
        }
    };

    // Tapping a cover card opens Item Detail (ui-spec's real "tap -> Item detail -> Play" flow) by
    // swapping the window's content in — the same content-swap mechanism the mini-bar's
    // tap-to-expand already uses for Player (there's no `AdwNavigationView`/`AdwDialog` at this
    // crate's libadwaita ceiling). Constructed on demand, per tap, exactly mirroring how the
    // mini-bar gesture below builds a fresh `PlayerScreen` on every open rather than keeping one
    // around; `on_back` restores `root` (this shell) the same way Player's `on_collapse` does.
    // Tapping Play/Resume or a chapter row, though, doesn't go through `on_back` at all — `on_play`
    // (below) is `start_playback`, which opens the full Player screen itself once playback has
    // actually started, rather than returning to the shelf and waiting for the mini bar to catch
    // up a moment later.
    // Filled in right after `library_screen` is actually built, below — `on_open` (this closure)
    // needs to call a method on it from inside Item Detail's series-button callback, but
    // `library_screen` itself is built from a call that takes `on_open.clone()` as its own
    // card-tap handler, so it can't exist yet when `on_open` is defined. Safe because
    // `library_screen` is always built synchronously later in this same `build()` call, before
    // the event loop ever runs — no tap into Item Detail's series button can happen before this
    // cell is populated.
    let library_screen_cell: Rc<RefCell<Option<screens::library::LibraryScreen>>> = Rc::new(RefCell::new(None));

    let on_open = {
        let window = window.clone();
        let root = root.clone();
        let pool = pool.clone();
        let server = server.clone();
        let account = account.clone();
        let session = session.clone();
        let download_manager = download_manager.clone();
        let controller = mini_bar.controller.clone();
        let open_player = open_player.clone();
        let stack = stack.clone();
        let library_screen_cell = library_screen_cell.clone();
        let start_playback = Rc::new(start_playback);
        move |request: PlayRequest| {
            let on_play = {
                let start_playback = start_playback.clone();
                let request = request.clone();
                move |item_id: String, start_chapter: Option<usize>| {
                    debug_assert_eq!(item_id, request.item_id, "Item Detail should only ever report its own item id back");
                    start_playback(request.clone(), start_chapter);
                }
            };
            let on_back = {
                let window = window.clone();
                let root = root.clone();
                move || crate::widgets::swap_content(&window, &root)
            };
            let on_open_player = {
                let open_player = open_player.clone();
                move || open_player()
            };
            // Closes Item Detail, restores the shell, switches to the Library tab, and filters
            // it to just this series — the same "close this screen, land on a specific Library
            // state" shape `on_open_shelf` (below) already uses for Home's shelf tap-through.
            let on_open_series = {
                let window = window.clone();
                let root = root.clone();
                let stack = stack.clone();
                let library_screen_cell = library_screen_cell.clone();
                move |series_name: String| {
                    crate::widgets::swap_content(&window, &root);
                    stack.set_visible_child_name("library");
                    if let Some(library_screen) = library_screen_cell.borrow().as_ref() {
                        library_screen.apply_series_filter(&series_name);
                    }
                }
            };
            // "View" on a "Download started" toast (`widgets::download_scope_menu`'s own doc):
            // closes whichever screen raised it and lands on the Downloads tab, the same
            // "close this screen, land on a specific tab" shape `on_open_series`/`on_open_shelf`
            // already use.
            let on_open_downloads = {
                let window = window.clone();
                let root = root.clone();
                let stack = stack.clone();
                move || {
                    crate::widgets::swap_content(&window, &root);
                    stack.set_visible_child_name("downloads");
                }
            };
            let item_detail_screen = screens::item_detail::build(
                pool.clone(),
                server.clone(),
                account.clone(),
                session.clone(),
                download_manager.clone(),
                controller.clone(),
                request.item_id.clone(),
                on_play,
                on_back,
                on_open_player,
                on_open_series,
                on_open_downloads,
            );
            crate::widgets::swap_content(&window, &item_detail_screen.root);
        }
    };

    // Library is built before Home only so the tap-through closure below can capture the
    // already-built screen; the `add_titled_with_icon` calls (not build order) fix the
    // switcher's tab order — Home stays first.
    let library_screen = screens::library::build(
        pool.clone(),
        paths.clone(),
        server.clone(),
        account.clone(),
        session.clone(),
        offline_mode.clone(),
        on_open.clone(),
        on_relogin.clone(),
        {
            let stack = stack.clone();
            move || stack.set_visible_child_name("settings")
        },
    );
    *library_screen_cell.borrow_mut() = Some(library_screen.clone());

    // Home's shelf headings' tap-through (ui-spec Home section): switch to Library and land on
    // the shelf's view — Recently Added pre-sorted by date added; Continue Listening pre-sorted
    // by last listened and filtered to in-progress books. Session-transient both, exactly like
    // manual sort/filter changes — navigation-with-intent, not a preference.
    let on_open_shelf = {
        let stack = stack.clone();
        let library_screen = library_screen.clone();
        move |shelf| {
            stack.set_visible_child_name("library");
            match shelf {
                crate::screens::home::Shelf::RecentlyAdded => library_screen.apply_view(crate::screens::library::SortKey::DateAdded, false),
                crate::screens::home::Shelf::ContinueListening => library_screen.apply_view(crate::screens::library::SortKey::LastListened, true),
            }
        }
    };

    let home_screen = screens::home::build(
        pool.clone(),
        paths.clone(),
        server.clone(),
        account.clone(),
        session.clone(),
        offline_mode.clone(),
        on_open.clone(),
        on_relogin,
        on_open_shelf,
    );
    // Both shelves' badges follow downloads finishing or being cleared in this session.
    home_screen.follow_downloads(&download_manager);
    library_screen.follow_downloads(&download_manager);
    home_screen.follow_player(&mini_bar.controller);
    home_screen.follow_low_memory_mode(&low_memory_mode);
    library_screen.follow_low_memory_mode(&low_memory_mode);
    stack.add_titled_with_icon(&home_screen.root, Some("home"), &tr("Home"), "go-home-symbolic");
    stack.add_titled_with_icon(&library_screen.root, Some("library"), &tr("Library"), crate::icons::LIBRARY);
    let downloads_screen = screens::downloads::build(pool.clone(), paths.clone(), server, account, session.clone(), download_manager.clone(), window.clone(), Rc::new(on_open.clone()));
    stack.add_titled_with_icon(&downloads_screen.root, Some("downloads"), &tr("Downloads"), "folder-download-symbolic");

    // A dot on the Downloads tab while anything is downloading, so "something is running in the
    // background" stays visible from Home/Library too, once the user has navigated away from
    // wherever they started it (the toast's own "View" action, above, covers the moment right
    // after starting it). Cleared the moment the tab is actually opened.
    {
        let stack = stack.clone();
        let downloads_root = downloads_screen.root.clone();
        let download_manager = download_manager.clone();
        download_manager.add_listener({
            let stack = stack.clone();
            let downloads_root = downloads_root.clone();
            let download_manager = download_manager.clone();
            move |_event| {
                // Deferred: `download_manager.any_in_flight()` borrows the same `RefCell` several
                // publish() call sites are still holding mutably borrowed at the point they
                // publish (see `widgets::download_progress`'s identical fix for the same reason).
                let stack = stack.clone();
                let downloads_root = downloads_root.clone();
                let download_manager = download_manager.clone();
                glib::idle_add_local_once(move || {
                    let showing_downloads = stack.visible_child_name().as_deref() == Some("downloads");
                    stack.page(&downloads_root).set_needs_attention(download_manager.any_in_flight() && !showing_downloads);
                });
            }
        });
        // A download just finished: the radio is awake from it, so waiting listening sessions
        // can go along. Deferred for the same reason as above.
        download_manager.add_listener({
            let controller = mini_bar.controller.downgrade();
            let session = session.clone();
            move |event| {
                if !matches!(event, crate::downloads::DownloadEvent::ItemStateChanged { state: crate::downloads::ItemDownloadState::Complete, .. }) {
                    return;
                }
                let controller = controller.clone();
                let session = session.clone();
                glib::idle_add_local_once(move || {
                    if let Some(controller) = controller.upgrade() {
                        controller.send_listening_sessions(&session, crate::listening::SendReason::Opportunistic);
                    }
                });
            }
        });
        stack.connect_visible_child_name_notify(move |stack| {
            if stack.visible_child_name().as_deref() == Some("downloads") {
                stack.page(&downloads_root).set_needs_attention(false);
            }
        });
    }
    let settings_screen = screens::settings::build(
        pool.clone(),
        mini_bar.controller.clone(),
        download_manager.clone(),
        playback_settings,
        low_memory_mode.clone(),
        theme,
        paths,
        servers_with_accounts,
        window.clone(),
    );
    stack.add_titled_with_icon(&settings_screen.root, Some("settings"), &tr("Settings"), crate::icons::SETTINGS);

    let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).reveal(true).build();

    shell_box.append(&stack);
    shell_box.append(&mini_bar.root);
    shell_box.append(&switcher_bar);

    // Keyboard actions for the whole shell (ui-spec §6's global table; the accelerators
    // themselves are set app-wide in `application.rs`). Transport keys no-op when nothing is
    // loaded — every `PlayerController` method already does. Single-key bindings (`space` here)
    // are installed by `application::add_single_key_shortcuts` as a bubble-phase controller, so
    // a focused text entry types the key first and the action never fires: no per-widget guards
    // are wanted here. (They must not be plain app accelerators — GTK4 runs those in the capture
    // phase, ahead of the focused widget, which once swallowed a bare key in the Library search.)
    let play_pause_action = gtk4::gio::SimpleAction::new("play-pause", None);
    play_pause_action.connect_activate({
        let controller = mini_bar.controller.clone();
        move |_, _| controller.toggle_play_pause()
    });
    window.add_action(&play_pause_action);

    let switch_tab_action = gtk4::gio::SimpleAction::new("switch-tab", Some(gtk4::glib::VariantTy::STRING));
    switch_tab_action.connect_activate({
        let stack = stack.clone();
        move |_, parameter| {
            let Some(name) = parameter.and_then(|p| p.str()) else { return };
            // Unknown names are ignored rather than an error: the action exists so accelerators
            // can target it, not as a general navigation API.
            if stack.child_by_name(name).is_some() {
                stack.set_visible_child_name(name);
            }
        }
    });
    window.add_action(&switch_tab_action);

    let open_library_search_action = gtk4::gio::SimpleAction::new("open-library-search", None);
    open_library_search_action.connect_activate({
        let stack = stack.clone();
        let search_entry = library_screen.search_entry.clone();
        move |_, _| {
            stack.set_visible_child_name("library");
            search_entry.grab_focus();
        }
    });
    window.add_action(&open_library_search_action);

    // A single `GestureDrag` recognizes both a tap (negligible movement) and a swipe up past
    // `SWIPE_UP_MIN_DISTANCE_PX` — one gesture controller, not two competing ones claiming the
    // same touch sequence on `mini_bar.root`. Tap-to-open (MP-5) is the guaranteed fallback per
    // the ui-spec regardless of how the swipe (MP-6) ends up calibrated, and the swipe threshold
    // here is a first pass only: the ui-spec flags this hit-zone as "not yet validated" against
    // phosh's own bottom-edge shell gestures, so it needs on-device calibration before being
    // final, same posture `widgets/pull_to_refresh.rs` documents for its own hand-rolled gesture.
    // Deliberately no live drag-follow animation: the spec only requires that a swipe opens the
    // player "same as tapping," and building drag-following polish ahead of that calibration
    // would likely just need to be re-tuned or thrown away once real hardware is measured.
    let mini_bar_gesture = gtk4::GestureDrag::new();
    mini_bar_gesture.connect_drag_end({
        let open_player = open_player.clone();
        move |gesture, _, _| {
            if let Some((offset_x, offset_y)) = gesture.offset() {
                if mini_bar_gesture_should_open(offset_x, offset_y) {
                    open_player();
                }
            }
        }
    });
    mini_bar.root.add_controller(mini_bar_gesture);
    mini_bar.set_on_open({
        let open_player = open_player.clone();
        move || open_player()
    });

    #[cfg(test)]
    let toast_overlay_hook = root.clone();

    steps.finish("built", None);
    MainWindow {
        root: root.upcast(),
        _call_watcher: call_watcher,
        _route_watcher: route_watcher,
        _connectivity_watcher: connectivity_watcher,
        download_manager,
        controller: mini_bar.controller.clone(),
        _shell_alive: shell_alive,
        #[cfg(test)]
        hooks: TestHooks {
            stack,
            switcher_bar,
            mini_bar: mini_bar.hooks,
            open_player: open_player.clone(),
            switch_tab: switch_tab_action,
            play_pause: play_pause_action,
            open_library_search: open_library_search_action,
            library_search: library_screen.search_entry,
            pause_on_unplug_switch: settings_screen.hooks.pause_on_unplug_switch,
            toast_overlay: toast_overlay_hook,
            resume_on_replug_switch: settings_screen.hooks.resume_on_replug_switch,
        },
    }
}

/// What to toast, if anything, for one progress-sync outcome. `last_failure` carries the
/// current failure episode between calls: a success ends it, a repeat of the same failure stays
/// quiet, and a different kind of failure toasts again.
pub(crate) fn progress_sync_toast(last_failure: &Cell<Option<player::ProgressSyncOutcome>>, outcome: player::ProgressSyncOutcome) -> Option<&'static str> {
    use player::ProgressSyncOutcome;
    if outcome == ProgressSyncOutcome::Synced {
        last_failure.set(None);
        return None;
    }
    if last_failure.replace(Some(outcome)) == Some(outcome) {
        return None;
    }
    Some(match outcome {
        ProgressSyncOutcome::SessionExpired => tr_noop("Session expired — progress isn't syncing. Log in again"),
        _ => tr_noop("Failed to sync progress — will retry"),
    })
}

#[cfg(test)]
mod progress_sync_toast_tests {
    use std::cell::Cell;

    use super::progress_sync_toast;
    use crate::player::ProgressSyncOutcome::{Failed, SessionExpired, Synced};

    #[test]
    fn toasts_once_per_failure_episode() {
        let last = Cell::new(None);
        assert_eq!(progress_sync_toast(&last, Synced), None);
        assert_eq!(progress_sync_toast(&last, Failed), Some("Failed to sync progress — will retry"));
        assert_eq!(progress_sync_toast(&last, Failed), None, "a repeat of the same failure stays quiet");
        assert_eq!(progress_sync_toast(&last, Failed), None);
        assert_eq!(progress_sync_toast(&last, Synced), None, "a success ends the episode quietly");
        assert_eq!(progress_sync_toast(&last, Failed), Some("Failed to sync progress — will retry"), "a new episode toasts again");
    }

    #[test]
    fn a_different_kind_of_failure_toasts_again() {
        let last = Cell::new(None);
        assert!(progress_sync_toast(&last, Failed).is_some());
        assert_eq!(
            progress_sync_toast(&last, SessionExpired),
            Some("Session expired — progress isn't syncing. Log in again"),
            "an expired session is news even mid-outage"
        );
        assert_eq!(progress_sync_toast(&last, SessionExpired), None);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::{pool, pump_until};

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point. Points the Home tab's
    /// server at an address nothing listens on (`127.0.0.1:1`, immediate connection-refused) —
    /// this test only cares about the stack's shape, not sync behavior, so it must never make a
    /// real network call.
    pub(crate) fn run(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, "http://127.0.0.1:1")).unwrap();
        let account_id =
            runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool,
            crate::test_support::test_paths(),
            server,
            account,
            session,
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();

        for name in ["home", "library", "downloads", "settings"] {
            assert!(hooks.stack.child_by_name(name).is_some(), "missing destination: {name}");
        }
        assert!(hooks.switcher_bar.reveals(), "the tab bar should always be shown");
        assert!(!hooks.mini_bar.bar.is_visible(), "the mini bar should stay hidden until something plays");

        // The Settings tab is real now — its switches must reflect the `PlaybackSettings` the
        // shell was built with (the defaults here), so a user sees reality, not stale state.
        assert!(hooks.pause_on_unplug_switch.state(), "pause-on-unplug defaults to on");
        assert!(!hooks.resume_on_replug_switch.state(), "resume-on-replug defaults to off");

        // Keyboard actions (ui-spec §6), activated via their handles — the accel-to-action
        // mapping itself is GTK-level and can't be driven without real key events.
        hooks.switch_tab.activate(Some(&"settings".to_variant()));
        assert_eq!(hooks.stack.visible_child_name().as_deref(), Some("settings"));
        hooks.switch_tab.activate(Some(&"library".to_variant()));
        assert_eq!(hooks.stack.visible_child_name().as_deref(), Some("library"));

        // Ctrl+F's action focuses the search entry — which only takes hold on a mapped window
        // (same reason `run_chapters_sheet_lists_and_seeks` presents its window).
        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));
        hooks.open_library_search.activate(None::<&gtk4::glib::Variant>);
        assert_eq!(hooks.stack.visible_child_name().as_deref(), Some("library"), "search should land on the Library tab");
        pump_until(
            || gtk4::prelude::GtkWindowExt::focus(&app_window).is_some_and(|focused| focused == *hooks.library_search.upcast_ref::<gtk4::Widget>()),
            std::time::Duration::from_secs(5),
        );

        // Transport keys with nothing loaded are no-ops by contract (every `PlayerController`
        // method early-returns) — activating them must simply not panic.
        hooks.play_pause.activate(None::<&gtk4::glib::Variant>);
    }

    /// `handle_call_event`'s full behavior matrix — all the semantics the one-line closure
    /// `build()` registers with `ModemManagerCallWatcher` delegates to (the D-Bus-level call
    /// tracking lives in `abs_player::call_watch`'s own tests; its `FakeCallWatcher` is
    /// `#[cfg(test)]`-only inside `abs-player` and so isn't visible across the crate boundary).
    pub(crate) fn run_call_interruption_wiring_pauses_playback(runtime: &tokio::runtime::Runtime) {
        use crate::player::tests::{account_and_server, insert_synced_item, mock_playable_item, test_backend};
        use crate::player::PlayRequest;
        use abs_player::call_watch::CallEvent;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));
        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let playing_controller = || {
            let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
            controller.start(abs_core::auth::Session::new(pool.clone(), &server, &account), PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None }, 1.0);
            crate::test_support::pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
            controller
        };
        let is_playing = |controller: &crate::player::PlayerController| controller.snapshot().unwrap().is_playing;
        let ringing = CallEvent::Started { incoming: true };
        let rejected = CallEvent::Ended { answered: false, outgoing: false };

        // The field report: ringing pauses straight away, not only once picked up — the build()
        // closure is reproduced and invoked exactly as registered.
        let controller = playing_controller();
        let on_event: Box<dyn Fn(CallEvent)> = {
            let controller = controller.clone();
            Box::new(move |event| controller.handle_call_event(event))
        };
        on_event(ringing);
        assert!(!is_playing(&controller), "an incoming call should pause as soon as it rings");
        // Rejected (or missed): playback picks up again.
        on_event(rejected);
        crate::test_support::pump_until(|| is_playing(&controller), std::time::Duration::from_secs(10));
        controller.stop();

        // Picked up, then hung up later: stays paused.
        let controller = playing_controller();
        controller.handle_call_event(ringing);
        controller.handle_call_event(CallEvent::Answered);
        assert!(!is_playing(&controller));
        controller.handle_call_event(CallEvent::Ended { answered: true, outgoing: false });
        assert!(!is_playing(&controller), "a call that was answered must not resume playback when it ends");
        controller.stop();

        // An outgoing call pauses when dialing and never resumes, even unanswered.
        let controller = playing_controller();
        controller.handle_call_event(CallEvent::Started { incoming: false });
        assert!(!is_playing(&controller), "dialing out should pause");
        controller.handle_call_event(CallEvent::Ended { answered: false, outgoing: true });
        assert!(!is_playing(&controller), "an outgoing call ending must not resume playback");
        controller.stop();

        // The user taking over while it rings — pausing again (via any path) or playing — means
        // the call's pause is no longer the reason; a later rejection changes nothing.
        let controller = playing_controller();
        controller.handle_call_event(ringing);
        controller.play();
        crate::test_support::pump_until(|| is_playing(&controller), std::time::Duration::from_secs(10));
        controller.pause();
        controller.handle_call_event(rejected);
        assert!(!is_playing(&controller), "a manual pause during the ringing must not be lifted by the call ending");
        controller.stop();

        // Already paused when the call rings: the call isn't the reason, so it's not resumed.
        let controller = playing_controller();
        controller.pause();
        controller.handle_call_event(ringing);
        controller.handle_call_event(rejected);
        assert!(!is_playing(&controller), "a call must never start playback that was already paused");
        controller.stop();

        // Call events with nothing loaded are no-ops by contract.
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.handle_call_event(ringing);
        controller.handle_call_event(rejected);
        assert!(controller.snapshot().is_none());
        controller.stop();
    }

    /// Same convention as `run_call_interruption_wiring_pauses_playback` above: `FakeConnectivityWatcher`
    /// is `#[cfg(test)]`-only inside `abs-player`, so this reproduces `build()`'s exact one-line
    /// closure (`move || controller.sync_pending_progress()`) and invokes it directly, rather than
    /// injecting a fake watcher into `MainWindow::build`.
    pub(crate) fn run_connectivity_restored_wiring_syncs_pending_progress(runtime: &tokio::runtime::Runtime) {
        use crate::player::tests::{account_and_server, insert_synced_item, mock_playable_item, test_backend};
        use crate::player::PlayRequest;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/me/progress/item-1"))
                .respond_with(wiremock::ResponseTemplate::new(404))
                .mount(&mock_server),
        );
        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        controller.start(session.clone(), PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None }, 1.0);
        crate::test_support::pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        crate::test_support::pump_until(|| false, std::time::Duration::from_millis(300));
        // Unreachable at the moment of pausing: that send fails, so the session is still waiting.
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/api/session/local-all"))
                .respond_with(wiremock::ResponseTemplate::new(503))
                .with_priority(1)
                .up_to_n_times(1)
                .mount(&mock_server),
        );
        controller.pause();
        let session_sends = || {
            runtime.block_on(mock_server.received_requests()).unwrap().into_iter().filter(|r| r.url.path() == "/api/session/local-all").count()
        };
        crate::test_support::pump_until(|| session_sends() == 1, std::time::Duration::from_secs(6));

        let requests_before = runtime.block_on(mock_server.received_requests()).unwrap().len();

        // The connectivity watcher's callback, as `build_main_window` wires it.
        let on_connectivity_restored: Box<dyn Fn()> = {
            let controller = controller.clone();
            let session = session.clone();
            Box::new(move || {
                controller.sync_pending_progress();
                controller.network_back(&session);
            })
        };
        on_connectivity_restored();
        crate::test_support::pump_until(|| false, std::time::Duration::from_millis(1500));

        let requests_after = runtime.block_on(mock_server.received_requests()).unwrap();
        let new_requests = &requests_after[requests_before..];
        let new_sends = new_requests.iter().filter(|r| r.url.path() == "/api/session/local-all").count();
        assert_eq!(new_sends, 1, "connectivity being restored should send the waiting session exactly once");
        assert_eq!(crate::player::tests::positions_sent(new_requests, "item-1").len(), 1);
        controller.stop();
    }

    /// `handle_route_event`'s full behavior matrix — all the semantics that the one-line closure
    /// `build()` registers with `PulseRouteWatcher` delegates to (the watcher's own
    /// audio-server-level classification lives in `abs-player`'s tests; the FakeRouteWatcher
    /// isn't visible across the crate boundary, exactly like the call watcher's fake).
    pub(crate) fn run_headphone_route_events_follow_the_settings(runtime: &tokio::runtime::Runtime) {
        use crate::player::tests::{account_and_server, insert_synced_item, mock_playable_item, test_backend};
        use crate::player::PlayRequest;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 8));
        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let start_book = |controller: &crate::player::PlayerController| {
            controller.start(
                abs_core::auth::Session::new(pool.clone(), &server, &account),
                PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
                1.0,
            );
        };
        let unplug = abs_player::route_watch::RouteEvent::Unplugged;
        let replug = abs_player::route_watch::RouteEvent::Replugged;

        // Defaults (pause on, resume off): unplug pauses, replug stays paused.
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.set_headphone_behavior(true, false);
        start_book(&controller);
        crate::test_support::pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        controller.handle_route_event(unplug);
        assert!(!controller.snapshot().unwrap().is_playing, "unplugging headphones should pause playback");
        controller.handle_route_event(replug);
        assert!(!controller.snapshot().unwrap().is_playing, "with resume off (the default), a replug stays paused");
        controller.stop();

        // Resume on: a replug undoes exactly the unplug pause.
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.set_headphone_behavior(true, true);
        start_book(&controller);
        crate::test_support::pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        controller.handle_route_event(unplug);
        assert!(!controller.snapshot().unwrap().is_playing, "unplugging headphones should pause playback");
        controller.handle_route_event(replug);
        crate::test_support::pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        controller.stop();

        // A manual pause is never overridden by a replug — even with resume on, and even when
        // the unplug happened while already paused (an unplug of a paused player is not a
        // pause *caused by* the unplug).
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.set_headphone_behavior(true, true);
        start_book(&controller);
        crate::test_support::pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        // The manual pause (also the path a phone call takes) clears the "paused by unplug" mark.
        controller.pause();
        controller.handle_route_event(unplug);
        controller.handle_route_event(replug);
        assert!(!controller.snapshot().unwrap().is_playing, "a replug must not resume a manually paused (or call-paused) book");
        controller.stop();

        // Pause-on-unplug off: unplugging changes nothing at all.
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.set_headphone_behavior(false, true);
        start_book(&controller);
        crate::test_support::pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        controller.handle_route_event(unplug);
        assert!(controller.snapshot().unwrap().is_playing, "with pause-on-unplug off, an unplug must not pause");
        controller.handle_route_event(replug);
        assert!(controller.snapshot().unwrap().is_playing);
        controller.stop();

        // Route events with nothing loaded are no-ops by contract.
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.set_headphone_behavior(true, true);
        controller.handle_route_event(unplug);
        controller.handle_route_event(replug);
        assert!(controller.snapshot().is_none());
        controller.stop();
    }

    /// Regression test for the reported bug: toggling offline mode on Home didn't move Library's
    /// toggle or its filtered list until the app restarted (and vice versa), because each screen
    /// kept its own private `Rc<Cell<bool>>` instead of sharing one `OfflineModeState`. Builds
    /// both `screens::home::build` and `screens::library::build` sharing one `OfflineModeState`
    /// and one pool — exactly as `main_window::build` wires them — and toggles each screen's own
    /// widget in turn, asserting the *other*, already-built screen's toggle/banner/filtered list
    /// update live, with neither screen being rebuilt.
    pub(crate) fn run_offline_mode_toggle_is_shared_between_home_and_library(runtime: &tokio::runtime::Runtime) {
        use crate::screens::home::tests::{account_and_server, count_children, item_json};
        use crate::screens::library::tests::list_box_titles;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary"), item_json("item-2", "Dune")]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);

        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());

        let home_screen = crate::screens::home::build(
            pool.clone(),
            crate::test_support::test_paths(),
            server.clone(),
            account.clone(),
            session.clone(),
            offline_mode.clone(),
            |_| {},
            || {},
            |_| {},
        );
        let library_screen = crate::screens::library::build(
            pool.clone(),
            crate::test_support::test_paths(),
            server.clone(),
            account.clone(),
            session,
            offline_mode,
            |_| {},
            || {},
            || {},
        );
        let home_hooks = home_screen.test_hooks();
        let library_hooks = library_screen.test_hooks();

        pump_until(|| count_children(&home_hooks.recent_row) == 2, std::time::Duration::from_secs(10));
        pump_until(|| library_hooks.list_box.row_at_index(1).is_some(), std::time::Duration::from_secs(10));

        // Mark "item-1" downloaded, same setup as each screen's own single-screen offline test.
        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-1", &[abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 3600.0, offset_seconds: 0.0, size_bytes: None }])).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::upsert_pending(&pool, &server.id, "item-1", "1", "/p/1.mp3")).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::mark_complete(&pool, &server.id, "item-1", "1", 10)).unwrap();

        // Toggle from HOME's widget only — Library must pick it up without being rebuilt.
        home_hooks.offline_toggle.set_active(true);
        pump_until(|| library_hooks.offline_toggle.is_active(), std::time::Duration::from_secs(5));
        assert!(library_hooks.offline_banner.reveals_child(), "Library's banner should reveal from a Home-driven toggle, with no rebuild");
        pump_until(|| list_box_titles(&library_hooks.list_box) == vec!["Project Hail Mary".to_string()], std::time::Duration::from_secs(5));
        assert!(home_hooks.offline_banner.reveals_child());

        // Toggle back off from LIBRARY's widget — Home must pick up the reverse.
        library_hooks.offline_toggle.set_active(false);
        pump_until(|| !home_hooks.offline_toggle.is_active(), std::time::Duration::from_secs(5));
        assert!(!home_hooks.offline_banner.reveals_child());
        pump_until(|| count_children(&home_hooks.recent_row) == 2, std::time::Duration::from_secs(5));
        assert_eq!(list_box_titles(&library_hooks.list_box).len(), 2);
    }

    /// The full chain restored by this plan: tapping a Home card opens Item Detail (not
    /// playback directly), Item Detail shows the tapped item's real metadata, and tapping its
    /// Play button both starts real playback and opens the full Player screen directly (not the
    /// shelf — the mini bar catching up a moment later no longer matters, since the user is
    /// already looking at the real Player screen) — same style as
    /// `run_offline_mode_toggle_is_shared_between_home_and_library`'s own cross-screen coverage.
    pub(crate) fn run_tapping_a_card_opens_item_detail_then_play_opens_the_player_screen(runtime: &tokio::runtime::Runtime) {
        use crate::screens::home::tests::item_json;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .mount(&mock_server),
        );
        // Item Detail's own chapters/tracks resolve, and the real audio Play actually loads.
        runtime.block_on(crate::player::tests::mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, &mock_server.uri())).unwrap();
        let account_id = runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token123", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool,
            crate::test_support::test_paths(),
            server,
            account,
            session,
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();

        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));

        let home_root = hooks.stack.child_by_name("home").expect("home tab exists");
        pump_until(|| find_card_button(&home_root).is_some(), std::time::Duration::from_secs(10));
        let card_button = find_card_button(&home_root).expect("a synced item's card should render");
        card_button.emit_clicked();

        // Item Detail should now be the window's content, with the tapped item's real title —
        // proving the tap opened Detail rather than starting playback directly.
        pump_until(
            || app_window.content().is_some_and(|content| find_label_text(&content, "Project Hail Mary")),
            std::time::Duration::from_secs(5),
        );
        assert!(!hooks.mini_bar.bar.is_visible(), "opening Item Detail must not itself start playback");

        // Tapping Play should start real playback and open the full Player screen directly.
        let content = app_window.content().expect("Item Detail should be showing");
        let play_button = find_button_labeled(&content, "Play").expect("Item Detail's Play button");
        play_button.emit_clicked();

        pump_until(|| hooks.mini_bar.bar.is_visible(), std::time::Duration::from_secs(10));
        pump_until(
            || app_window.content().is_some_and(|c| c != window.root && find_button_with_icon(&c, "go-down-symbolic").is_some()),
            std::time::Duration::from_secs(5),
        );
        let content = app_window.content().expect("the Player screen should be showing");
        assert!(content != window.root, "tapping Play should open the full Player screen, not restore the shell");
        assert!(
            find_button_with_icon(&content, "go-down-symbolic").is_some(),
            "the Player screen's collapse button should be present"
        );
        assert_eq!(hooks.mini_bar.title_label.label(), "Project Hail Mary", "the mini bar should reflect the item Play just started");
    }

    /// A real playback failure (the track file 404s) should toast once, shell-wide, regardless of
    /// which tab is open — and the toast's "View" action should open the Full Player screen,
    /// the same as tapping the mini bar. Starts playback directly via the controller (rather than
    /// through Home/Item Detail's UI, already covered above) since this test is about the toast,
    /// not the tap-through path.
    pub(crate) fn run_playback_error_toasts_once_with_a_details_action(runtime: &tokio::runtime::Runtime) {
        use crate::screens::home::tests::item_json;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/items/item-1"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "media": { "audioFiles": [{ "ino": "1", "duration": 5.0 }] }
                })))
                .mount(&mock_server),
        );
        // The track file 404s — metadata resolves fine, but the actual audio fetch fails once
        // GStreamer's `souphttpsrc` tries to read it, producing a real bus error.
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/items/item-1/file/1"))
                .respond_with(wiremock::ResponseTemplate::new(404))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, &mock_server.uri())).unwrap();
        let account_id = runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token123", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool,
            crate::test_support::test_paths(),
            server,
            account,
            session,
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();

        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));

        let home_root = hooks.stack.child_by_name("home").expect("home tab exists");
        pump_until(|| find_card_button(&home_root).is_some(), std::time::Duration::from_secs(10));
        find_card_button(&home_root).expect("a synced item's card should render").emit_clicked();

        pump_until(
            || app_window.content().is_some_and(|content| find_label_text(&content, "Project Hail Mary")),
            std::time::Duration::from_secs(5),
        );
        let content = app_window.content().expect("Item Detail should be showing");
        let play_button = find_button_labeled(&content, "Play").expect("Item Detail's Play button");
        play_button.emit_clicked();

        pump_until(
            || find_button_labeled(hooks.toast_overlay.upcast_ref(), "View").is_some(),
            std::time::Duration::from_secs(10),
        );
        let details_button =
            find_button_labeled(hooks.toast_overlay.upcast_ref(), "View").expect("a playback failure should toast with a View action");
        details_button.emit_clicked();

        pump_until(
            || app_window.content().is_some_and(|c| c != window.root && find_button_with_icon(&c, "go-down-symbolic").is_some()),
            std::time::Duration::from_secs(5),
        );
        assert!(
            app_window.content().is_some_and(|c| c != window.root),
            "the toast's View action should open the Full Player screen, same as the mini bar's own tap"
        );
    }

    /// End-to-end proof of Item Detail's series-button tap-through: tap a card with series
    /// metadata, open Item Detail, tap its series button once the real "N/M" has resolved,
    /// confirm the shell is restored, the Library tab is active, and the visible list is
    /// filtered to just that series.
    pub(crate) fn run_tapping_the_series_button_opens_library_filtered_to_that_series(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "item-1",
                        "addedAt": 1_700_000_000_000i64,
                        "media": { "duration": 3600.0, "metadata": { "title": "Foundation Book", "authorName": "Isaac Asimov", "seriesName": "Foundation" } }
                    }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(crate::player::tests::mock_playable_item(&mock_server, "item-1", 5));
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/series"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{ "id": "series-1", "name": "Foundation", "books": [{ "id": "item-1", "sequence": "1" }] }]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, &mock_server.uri())).unwrap();
        let account_id = runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token123", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool,
            crate::test_support::test_paths(),
            server,
            account,
            session,
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();

        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));

        let home_root = hooks.stack.child_by_name("home").expect("home tab exists");
        pump_until(|| find_card_button(&home_root).is_some(), std::time::Duration::from_secs(10));
        let card_button = find_card_button(&home_root).expect("a synced item's card should render");
        card_button.emit_clicked();

        pump_until(
            || app_window.content().is_some_and(|content| find_label_text(&content, "Foundation Book")),
            std::time::Duration::from_secs(5),
        );

        // Pass 1 shows the plain name first; wait for Pass 2's real "1/1" before tapping, so the
        // click is against the finished, network-refined state.
        pump_until(
            || app_window.content().is_some_and(|content| find_button_containing_label(&content, "Foundation, 1/1").is_some()),
            std::time::Duration::from_secs(5),
        );
        let content = app_window.content().expect("Item Detail should be showing");
        let series_button = find_button_containing_label(&content, "Foundation, 1/1").expect("the series button should show the real sequence/total");
        series_button.emit_clicked();

        pump_until(
            || app_window.content().is_some_and(|content| content == window.root),
            std::time::Duration::from_secs(5),
        );
        assert_eq!(hooks.stack.visible_child_name().as_deref(), Some("library"), "tapping the series button should switch to the Library tab");
        let library_root = hooks.stack.child_by_name("library").expect("library tab exists");
        pump_until(|| find_label_text(&library_root, "Foundation Book"), std::time::Duration::from_secs(5));
    }

    #[test]
    fn mini_bar_gesture_recognizes_a_tap() {
        assert!(mini_bar_gesture_should_open(0.0, 0.0));
        assert!(mini_bar_gesture_should_open(TAP_MAX_MOVEMENT_PX, -TAP_MAX_MOVEMENT_PX));
    }

    #[test]
    fn mini_bar_gesture_recognizes_a_clean_swipe_up() {
        assert!(mini_bar_gesture_should_open(0.0, -SWIPE_UP_MIN_DISTANCE_PX));
        assert!(mini_bar_gesture_should_open(2.0, -(SWIPE_UP_MIN_DISTANCE_PX + 1.0)));
    }

    #[test]
    fn mini_bar_gesture_ignores_a_swipe_down() {
        assert!(!mini_bar_gesture_should_open(0.0, SWIPE_UP_MIN_DISTANCE_PX));
    }

    #[test]
    fn mini_bar_gesture_ignores_a_horizontal_swipe() {
        assert!(!mini_bar_gesture_should_open(SWIPE_UP_MIN_DISTANCE_PX, 0.0));
    }

    #[test]
    fn mini_bar_gesture_ignores_a_diagonal_drag_that_is_mostly_horizontal() {
        // Crosses the vertical threshold, but the horizontal component dominates — not a clean
        // upward swipe, so this must not open the player.
        assert!(!mini_bar_gesture_should_open(SWIPE_UP_MIN_DISTANCE_PX * 2.0, -SWIPE_UP_MIN_DISTANCE_PX));
    }

    #[test]
    fn mini_bar_gesture_requires_the_full_swipe_distance() {
        assert!(!mini_bar_gesture_should_open(0.0, -(SWIPE_UP_MIN_DISTANCE_PX - 1.0)));
    }

    /// Direct proof that both of the mini bar's gesture outcomes — a tap (MP-5) and a swipe up
    /// past the threshold (MP-6) — reach the exact same navigation. There's no raw-gesture test
    /// harness in this codebase to synthesize a real `GestureDrag` sequence against, so this
    /// asserts on `TestHooks::open_player`, the shared closure both gesture outcomes call — the
    /// distance/direction decision itself is covered in isolation by the `mini_bar_gesture_*`
    /// tests above.
    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Where a screen reader lands: the
    /// shown tab's heading when the shell appears and on every tab switch (never the search
    /// entry, which only Ctrl+F focuses — and keeps), the play/pause button when the player
    /// opens, the tab's heading again when it collapses. Never with a focus ring.
    pub(crate) fn run_each_screen_focuses_its_heading_for_screen_readers(runtime: &tokio::runtime::Runtime) {
        use crate::a11y::tests::label_of;
        use crate::screens::home::tests::item_json;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(crate::player::tests::mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, &mock_server.uri())).unwrap();
        let account_id = runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token123", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool,
            crate::test_support::test_paths(),
            server,
            account,
            session,
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();
        app_window.set_content(Some(&window.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));

        let focused_name = || GtkWindowExt::focus(&app_window).and_then(|focus| label_of(&focus));
        pump_until(|| focused_name().as_deref() == Some("Home"), std::time::Duration::from_secs(5));
        assert_eq!(focused_name().as_deref(), Some("Home"), "the shell opens on Home's heading");
        assert!(!app_window.gets_focus_visible(), "no focus ring for a sighted user");

        hooks.switch_tab.activate(Some(&"library".to_variant()));
        pump_until(|| focused_name().as_deref() == Some("Library"), std::time::Duration::from_secs(5));
        assert!(!hooks.library_search.has_focus() && !GtkWindowExt::focus(&app_window).is_some_and(|focus| focus.is_ancestor(&hooks.library_search)), "a tab switch never focuses the search entry");

        hooks.open_library_search.activate(None);
        crate::test_support::run_pending_main_loop_work();
        pump_until(|| false, std::time::Duration::from_millis(200));
        assert!(GtkWindowExt::focus(&app_window).is_some_and(|focus| focus == hooks.library_search || focus.is_ancestor(&hooks.library_search)), "Ctrl+F's focus on the search entry is kept");

        for (name, heading) in [("downloads", "Downloads"), ("settings", "Settings"), ("home", "Home")] {
            hooks.switch_tab.activate(Some(&name.to_variant()));
            pump_until(|| focused_name().as_deref() == Some(heading), std::time::Duration::from_secs(5));
            assert_eq!(focused_name().as_deref(), Some(heading), "{name}");
        }

        // Into the player (through Item Detail's Play) and back.
        let home_root = hooks.stack.child_by_name("home").expect("home tab exists");
        pump_until(|| find_card_button(&home_root).is_some(), std::time::Duration::from_secs(10));
        find_card_button(&home_root).expect("a synced item's card should render").emit_clicked();
        pump_until(|| focused_name().as_deref() == Some("Project Hail Mary"), std::time::Duration::from_secs(5));
        assert_eq!(focused_name().as_deref(), Some("Project Hail Mary"), "Item Detail lands on the book's title");
        let content = app_window.content().expect("Item Detail should be showing");
        find_button_labeled(&content, "Play").expect("Item Detail's Play button").emit_clicked();
        pump_until(|| matches!(focused_name().as_deref(), Some("Pause" | "Play")), std::time::Duration::from_secs(10));
        assert!(matches!(focused_name().as_deref(), Some("Pause" | "Play")), "the player lands on play/pause, got {:?}", focused_name());
        assert!(!app_window.gets_focus_visible());
        WidgetExt::activate_action(&app_window, "player.collapse", None).ok();
        pump_until(|| focused_name().as_deref() == Some("Home"), std::time::Duration::from_secs(5));
        assert_eq!(focused_name().as_deref(), Some("Home"), "back on the shell, Home's heading again");

        // The mini bar's card: named after the book, and Enter on it opens the player again.
        assert_eq!(label_of(&hooks.mini_bar.focus_card).as_deref(), Some("Now playing: Project Hail Mary, Andy Weir"));
        assert!(hooks.mini_bar.focus_card.grab_focus());
        crate::a11y::tests::press_key(&hooks.mini_bar.focus_card, gtk4::gdk::Key::Return);
        pump_until(|| matches!(focused_name().as_deref(), Some("Pause" | "Play")), std::time::Duration::from_secs(10));
        assert!(matches!(focused_name().as_deref(), Some("Pause" | "Play")), "Enter on the card opened the player, got {:?}", focused_name());
        hooks.mini_bar.play_button.emit_clicked();
    }

    pub(crate) fn run_mini_bar_open_player_swaps_to_the_full_player_screen(runtime: &tokio::runtime::Runtime) {
        use crate::screens::home::tests::item_json;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(crate::player::tests::mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, &mock_server.uri())).unwrap();
        let account_id = runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token123", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool,
            crate::test_support::test_paths(),
            server,
            account,
            session,
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();

        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));

        // Start real playback so the mini bar has something to open into.
        let home_root = hooks.stack.child_by_name("home").expect("home tab exists");
        pump_until(|| find_card_button(&home_root).is_some(), std::time::Duration::from_secs(10));
        find_card_button(&home_root).expect("a synced item's card should render").emit_clicked();
        pump_until(
            || app_window.content().is_some_and(|content| find_label_text(&content, "Project Hail Mary")),
            std::time::Duration::from_secs(5),
        );
        let content = app_window.content().expect("Item Detail should be showing");
        find_button_labeled(&content, "Play").expect("Item Detail's Play button").emit_clicked();
        pump_until(|| hooks.mini_bar.bar.is_visible(), std::time::Duration::from_secs(10));
        // Set off from the content: a card (`card`) on a strip styled like the tab bar below it
        // (`mini-player`), not the plain `toolbar` it used to be.
        assert!(hooks.mini_bar.bar.has_css_class("mini-player"));
        assert!(!hooks.mini_bar.bar.has_css_class("toolbar"));
        assert!(hooks.mini_bar.card.has_css_class("card"));
        assert!(hooks.mini_bar.card.is_ancestor(&hooks.mini_bar.bar) && hooks.mini_bar.progress.is_ancestor(&hooks.mini_bar.card));

        // This is the same closure both the tap and swipe-up gesture outcomes call — invoking it
        // directly is the seam this test exercises (see the doc comment above).
        (hooks.open_player)();

        pump_until(
            || app_window.content().is_some_and(|content| content != window.root),
            std::time::Duration::from_secs(5),
        );
        assert!(
            app_window.content().is_some_and(|content| find_label_text(&content, "Project Hail Mary")),
            "opening the player from the mini bar should show the currently-playing item"
        );
    }

    /// Depth-first search for the first `GtkButton` that wraps an `item_card::build` card —
    /// identified structurally (a `GtkBox` whose first child is a `GtkOverlay`, the cover
    /// overlay every card has), since none of these buttons carry a name/id to search by.
    fn find_card_button(root: &gtk4::Widget) -> Option<gtk4::Button> {
        fn walk(widget: &gtk4::Widget, found: &mut Option<gtk4::Button>) {
            if found.is_some() {
                return;
            }
            if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
                let is_card = button
                    .child()
                    .and_then(|child| child.downcast::<gtk4::Box>().ok())
                    .and_then(|card_box| card_box.first_child())
                    .and_then(|first| first.downcast::<gtk4::Overlay>().ok())
                    .is_some();
                if is_card {
                    *found = Some(button.clone());
                    return;
                }
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                walk(&w, found);
                if found.is_some() {
                    return;
                }
                child = w.next_sibling();
            }
        }
        let mut found = None;
        walk(root, &mut found);
        found
    }

    /// Depth-first search for a `GtkLabel` with exactly this text anywhere under `root`.
    fn find_label_text(root: &gtk4::Widget, text: &str) -> bool {
        fn walk(widget: &gtk4::Widget, text: &str, found: &mut bool) {
            if *found {
                return;
            }
            if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
                if label.label() == text {
                    *found = true;
                    return;
                }
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                walk(&w, text, found);
                if *found {
                    return;
                }
                child = w.next_sibling();
            }
        }
        let mut found = false;
        walk(root, text, &mut found);
        found
    }

    /// Depth-first search for the first `GtkButton` with exactly this label anywhere under `root`.
    fn find_button_labeled(root: &gtk4::Widget, label: &str) -> Option<gtk4::Button> {
        fn walk(widget: &gtk4::Widget, label: &str, found: &mut Option<gtk4::Button>) {
            if found.is_some() {
                return;
            }
            if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
                if button.label().as_deref() == Some(label) {
                    *found = Some(button.clone());
                    return;
                }
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                walk(&w, label, found);
                if found.is_some() {
                    return;
                }
                child = w.next_sibling();
            }
        }
        let mut found = None;
        walk(root, label, &mut found);
        found
    }

    /// Like [`find_button_labeled`], but for a button built with a custom `Label` child (via
    /// `set_child`, not the `label` convenience property) — Item Detail's series button is
    /// exactly this shape, since its text updates in place across two render passes.
    fn find_button_containing_label(root: &gtk4::Widget, text: &str) -> Option<gtk4::Button> {
        fn walk(widget: &gtk4::Widget, text: &str, found: &mut Option<gtk4::Button>) {
            if found.is_some() {
                return;
            }
            if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
                if let Some(child) = button.child() {
                    if find_label_text(&child, text) {
                        *found = Some(button.clone());
                        return;
                    }
                }
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                walk(&w, text, found);
                if found.is_some() {
                    return;
                }
                child = w.next_sibling();
            }
        }
        let mut found = None;
        walk(root, text, &mut found);
        found
    }

    /// Regression test for "no route to the Downloads tab from Item Detail/Player" — both are
    /// content-swapped over the shell, hiding the tab bar, so the "Download started" toast's
    /// "View" action (`widgets::download_scope_menu`'s `started_download_toast`) is the only way
    /// back short of navigating there blind and hoping. Clicking it must close Item Detail and
    /// land on the Downloads tab. Also covers the Downloads tab's attention dot: it must appear
    /// while a batch is in flight and the tab isn't visible, and clear once it becomes visible.
    pub(crate) fn run_download_started_toast_view_action_opens_downloads(runtime: &tokio::runtime::Runtime) {
        use crate::screens::home::tests::item_json;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(crate::downloads::tests::mock_two_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, &mock_server.uri())).unwrap();
        let account_id = runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token123", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool,
            crate::test_support::test_paths(),
            server,
            account,
            session,
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();

        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));

        let home_root = hooks.stack.child_by_name("home").expect("home tab exists");
        pump_until(|| find_card_button(&home_root).is_some(), std::time::Duration::from_secs(10));
        find_card_button(&home_root).expect("a synced item's card should render").emit_clicked();
        pump_until(
            || app_window.content().is_some_and(|content| find_label_text(&content, "Project Hail Mary")),
            std::time::Duration::from_secs(5),
        );
        let content = app_window.content().expect("Item Detail should be showing");

        let download_button = find_menu_button_with_icon(&content, "folder-download-symbolic").expect("Item Detail's download button");
        download_button.popup();
        pump_until(|| find_button_containing_label(&content, "Entire book").is_some(), std::time::Duration::from_secs(2));
        find_button_containing_label(&content, "Entire book").expect("the Entire book scope row").emit_clicked();

        // Also covers the Downloads tab's attention dot (A4): while a batch is in flight and the
        // user is looking at Item Detail, not the Downloads tab, the tab should carry it.
        let downloads_root = hooks.stack.child_by_name("downloads").expect("downloads tab exists");
        pump_until(|| hooks.stack.page(&downloads_root).needs_attention(), std::time::Duration::from_secs(5));
        assert!(hooks.stack.page(&downloads_root).needs_attention(), "the Downloads tab should show a dot while something is downloading");

        pump_until(|| find_button_labeled(&content, "View").is_some(), std::time::Duration::from_secs(5));
        find_button_labeled(&content, "View").expect("the toast's View action").emit_clicked();

        pump_until(
            || app_window.content().is_some_and(|current| current == window.root),
            std::time::Duration::from_secs(5),
        );
        assert_eq!(hooks.stack.visible_child_name().as_deref(), Some("downloads"), "the toast's View action should switch to the Downloads tab");
        pump_until(|| !hooks.stack.page(&downloads_root).needs_attention(), std::time::Duration::from_secs(5));
        assert!(!hooks.stack.page(&downloads_root).needs_attention(), "opening the Downloads tab should clear its own attention dot");
    }

    /// Depth-first search for the first `GtkButton` constructed from exactly this icon name
    /// anywhere under `root` — used to prove the Player screen (not Item Detail or the shell) is
    /// showing, via its collapse button (`player.rs`'s `gtk4::Button::from_icon_name
    /// ("go-down-symbolic")`).
    fn find_button_with_icon(root: &gtk4::Widget, icon_name: &str) -> Option<gtk4::Button> {
        fn walk(widget: &gtk4::Widget, icon_name: &str, found: &mut Option<gtk4::Button>) {
            if found.is_some() {
                return;
            }
            if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
                if button.icon_name().as_deref() == Some(icon_name) {
                    *found = Some(button.clone());
                    return;
                }
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                walk(&w, icon_name, found);
                if found.is_some() {
                    return;
                }
                child = w.next_sibling();
            }
        }
        let mut found = None;
        walk(root, icon_name, &mut found);
        found
    }

    /// Like [`find_button_with_icon`], but for a `GtkMenuButton` (Item Detail/Player's download
    /// button, `folder-download-symbolic`) rather than a plain `GtkButton`.
    fn find_menu_button_with_icon(root: &gtk4::Widget, icon_name: &str) -> Option<gtk4::MenuButton> {
        fn walk(widget: &gtk4::Widget, icon_name: &str, found: &mut Option<gtk4::MenuButton>) {
            if found.is_some() {
                return;
            }
            if let Some(button) = widget.downcast_ref::<gtk4::MenuButton>() {
                if button.icon_name().as_deref() == Some(icon_name) {
                    *found = Some(button.clone());
                    return;
                }
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                walk(&w, icon_name, found);
                if found.is_some() {
                    return;
                }
                child = w.next_sibling();
            }
        }
        let mut found = None;
        walk(root, icon_name, &mut found);
        found
    }

    /// Saves what `window` currently shows as a PNG — for the visual check of the mini bar's
    /// look (see `run_mini_bar_screenshots`).
    pub(crate) fn save_window_png(window: &adw::ApplicationWindow, path: &std::path::Path) {
        let (width, height) = (window.width(), window.height());
        let paintable = gtk4::WidgetPaintable::new(Some(window));
        let snapshot = gtk4::Snapshot::new();
        gtk4::gdk::prelude::PaintableExt::snapshot(&paintable, &snapshot, f64::from(width), f64::from(height));
        let node = snapshot.to_node().expect("the window should have drawn something");
        let renderer = window.renderer().expect("a realized window has a renderer");
        let bounds = gtk4::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
        renderer.render_texture(&node, Some(&bounds)).save_to_png(path).expect("the screenshot should save");
    }

    /// The Library's "In progress only" switch, inside the view-options button's popover.
    fn in_progress_only_switch(library_root: &gtk4::Widget) -> Option<gtk4::Switch> {
        fn walk<T: IsA<gtk4::Widget>>(widget: &gtk4::Widget, found: &mut Vec<T>) {
            if let Some(hit) = widget.downcast_ref::<T>() {
                found.push(hit.clone());
            }
            let mut child = widget.first_child();
            while let Some(c) = child {
                walk(&c, found);
                child = c.next_sibling();
            }
        }
        let mut menu_buttons: Vec<gtk4::MenuButton> = Vec::new();
        walk(library_root, &mut menu_buttons);
        let popover = menu_buttons.iter().find(|b| b.tooltip_text().is_some_and(|t| t.contains("View options")))?.popover()?;
        let mut rows: Vec<adw::ActionRow> = Vec::new();
        walk(popover.upcast_ref(), &mut rows);
        let row = rows.into_iter().find(|r| r.title() == "In progress only")?;
        let mut switches: Vec<gtk4::Switch> = Vec::new();
        walk(row.upcast_ref(), &mut switches);
        switches.into_iter().next()
    }

    /// A solid-colour cover with a darker band, encoded as PNG — enough for covers to look like
    /// covers in screenshots.
    fn cover_png(rgb: [u8; 3]) -> Vec<u8> {
        let image = image::RgbImage::from_fn(300, 300, |_, y| {
            if (200..240).contains(&y) {
                image::Rgb(rgb.map(|c| c / 2))
            } else {
                image::Rgb(rgb)
            }
        });
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    /// Not a check: renders Home, Library, the full player, Home again after it, and Item Detail,
    /// with a long-titled book loaded, in light and dark, at a phone's window size, into
    /// `$ABS_SCREENSHOT_DIR` (does nothing when that isn't set) — so a change to how these screens
    /// look or size can be seen before it reaches a phone.
    pub(crate) fn run_mini_bar_screenshots(runtime: &tokio::runtime::Runtime) {
        let Some(dir) = std::env::var_os("ABS_SCREENSHOT_DIR").map(std::path::PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();

        let books = [
            ("item-1", "Krzysztof Kolumb. Odkrywca z wyspy Chios", "Jarosław Molenda", [230, 200, 40]),
            ("item-2", "Project Hail Mary", "Andy Weir", [40, 90, 160]),
            ("item-3", "The Left Hand of Darkness", "Ursula K. Le Guin", [120, 60, 140]),
            ("item-4", "Piranesi", "Susanna Clarke", [60, 140, 120]),
            ("item-5", "Surely You're Joking, Mr. Feynman!", "Richard Feynman", [200, 70, 50]),
            ("item-6", "Hyperion", "Dan Simmons", [30, 40, 60]),
            ("item-7", "Ghost in the Wires", "Kevin Mitnick", [20, 120, 140]),
            ("item-8", "The Dispossessed", "Ursula K. Le Guin", [190, 130, 60]),
            ("item-9", "Anathem", "Neal Stephenson", [90, 100, 110]),
            ("item-10", "Walkaway", "Cory Doctorow", [240, 110, 30]),
        ];
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        let results: Vec<_> = books
            .iter()
            .enumerate()
            .map(|(index, (id, title, author, _))| {
                serde_json::json!({
                    "id": id,
                    "addedAt": 1_700_000_000_000i64 - index as i64 * 1000,
                    "media": { "duration": 36000.0 + index as f64 * 1800.0, "metadata": { "title": title, "authorName": author } }
                })
            })
            .collect();
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": results })))
                .mount(&mock_server),
        );
        for (id, _, _, rgb) in &books {
            runtime.block_on(
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path(format!("/api/items/{id}/cover")))
                    .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(cover_png(*rgb), "image/png"))
                    .mount(&mock_server),
            );
        }
        // Chaptered, so the player's previous/next chapter buttons are in the shots.
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(&mock_server, "item-1", 60, &[("Prologue", 0.0, 20.0), ("One", 20.0, 60.0)]));

        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, &mock_server.uri())).unwrap();
        let account_id = runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token123", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        // A Librem 5's usable window under phosh: 720 logical px, less the top and bottom bars.
        let app_window = adw::ApplicationWindow::builder().default_width(360).default_height(648).build();
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool.clone(),
            crate::test_support::test_paths(),
            server.clone(),
            account.clone(),
            session,
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();
        app_window.set_content(Some(&window.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));

        let home_root = hooks.stack.child_by_name("home").expect("home tab exists");
        pump_until(|| find_card_button(&home_root).is_some(), std::time::Duration::from_secs(10));

        // A book in the mini bar, a little way in, paused (the play glyph, as on the phone).
        window.controller.start(
            abs_core::auth::Session::new(pool, &server, &account),
            PlayRequest {
                item_id: "item-1".to_string(),
                title: "Krzysztof Kolumb. Odkrywca z wyspy Chios".to_string(),
                author: Some("Jarosław Molenda".to_string()),
            },
            1.0,
        );
        pump_until(|| window.controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        pump_until(|| false, std::time::Duration::from_secs(4));
        window.controller.toggle_play_pause();
        // Covers decode off the main thread; give them time to land everywhere.
        pump_until(|| false, std::time::Duration::from_secs(3));

        let style = adw::StyleManager::default();
        let shot = |name: &str| {
            pump_until(|| false, std::time::Duration::from_millis(800));
            save_window_png(&app_window, &dir.join(format!("{name}.png")));
        };
        for (scheme, theme) in [(adw::ColorScheme::ForceLight, "light"), (adw::ColorScheme::ForceDark, "dark")] {
            style.set_color_scheme(scheme);
            hooks.stack.set_visible_child_name("home");
            shot(&format!("home-{theme}"));
            hooks.stack.set_visible_child_name("library");
            pump_until(|| false, std::time::Duration::from_secs(2));
            shot(&format!("library-{theme}"));
        }

        // The Library with a filter on: the view-options button shows its filter-active icon.
        let library_root = hooks.stack.child_by_name("library").expect("library tab exists");
        in_progress_only_switch(&library_root).expect("the view options' In progress only switch").set_active(true);
        for (scheme, theme) in [(adw::ColorScheme::ForceLight, "light"), (adw::ColorScheme::ForceDark, "dark")] {
            style.set_color_scheme(scheme);
            hooks.stack.set_visible_child_name("library");
            shot(&format!("library-filtered-{theme}"));
        }

        // The full player, then Home again: the window must not stay grown by the player.
        for (scheme, theme) in [(adw::ColorScheme::ForceLight, "light"), (adw::ColorScheme::ForceDark, "dark")] {
            style.set_color_scheme(scheme);
            hooks.stack.set_visible_child_name("home");
            (hooks.open_player)();
            pump_until(|| app_window.content().is_some_and(|c| c != window.root), std::time::Duration::from_secs(5));
            shot(&format!("player-{theme}"));
            find_button_with_icon(&app_window.content().unwrap(), "go-down-symbolic").expect("collapse").emit_clicked();
            pump_until(|| app_window.content().is_some_and(|c| c == window.root), std::time::Duration::from_secs(5));
            shot(&format!("home-after-player-{theme}"));
        }

        hooks.stack.set_visible_child_name("home");
        pump_until(|| false, std::time::Duration::from_millis(300));
        find_card_button(&home_root).expect("a card").emit_clicked();
        pump_until(|| app_window.content().is_some_and(|c| c != window.root), std::time::Duration::from_secs(5));
        pump_until(|| false, std::time::Duration::from_secs(2));
        for (scheme, theme) in [(adw::ColorScheme::ForceDark, "dark"), (adw::ColorScheme::ForceLight, "light")] {
            style.set_color_scheme(scheme);
            shot(&format!("item-detail-{theme}"));
        }
    }

    /// The usable height phosh leaves an app window on a Librem 5 (720 logical px, less the top
    /// and bottom bars) is ~648 px; this keeps a margin for other shells and scales.
    const PHONE_MAX_MIN_HEIGHT: i32 = 600;

    /// Every screen `widgets::swap_content` can put in the window must be able to get as short as
    /// a phone screen: the window grows to the largest minimum height any screen ever had and
    /// never shrinks back, so one too-tall screen (e.g. the full player for a long title) leaves
    /// Home's tab bar off the bottom of the screen for the rest of the session. Measured at a
    /// phone's 360 px width, in the states that make each screen tallest.
    pub(crate) fn run_every_screen_fits_a_phone(runtime: &tokio::runtime::Runtime) {
        const LONG_TITLE: &str = "Krzysztof Kolumb. Odkrywca z wyspy Chios — powieść historyczna w trzech tomach, wydanie drugie";
        const LONG_AUTHOR: &str = "Jarosław Molenda, Anna Kowalska-Nowakowska, Grzegorz Brzęczyszczykiewicz";
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        let results: Vec<_> = [("item-1", LONG_TITLE), ("item-2", LONG_TITLE), ("item-3", "Piranesi")]
            .iter()
            .map(|(id, title)| {
                serde_json::json!({
                    "id": id,
                    "addedAt": 1_700_000_000_000i64,
                    "media": { "duration": 36000.0, "metadata": { "title": title, "authorName": LONG_AUTHOR } }
                })
            })
            .collect();
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": results })))
                .mount(&mock_server),
        );
        // item-1's audio is gone from the server (an error banner on the player); item-2 and
        // item-3 play.
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/items/item-1"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "media": { "audioFiles": [{ "ino": "1", "duration": 60.0 }] }
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/items/item-1/file/1"))
                .respond_with(wiremock::ResponseTemplate::new(404))
                .mount(&mock_server),
        );
        // Chaptered, so the player's transport row has all five buttons when its width is checked.
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(&mock_server, "item-2", 60, &[("One", 0.0, 30.0), ("Two", 30.0, 60.0)]));
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(&mock_server, "item-3", 60, &[("One", 0.0, 30.0), ("Two", 30.0, 60.0)]));

        let pool = runtime.block_on(pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, &mock_server.uri())).unwrap();
        let account_id = runtime.block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token123", None)).unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().default_width(360).default_height(640).build();
        let servers_with_accounts = vec![(server.clone(), vec![account.clone()])];
        let window = build(
            pool.clone(),
            crate::test_support::test_paths(),
            server.clone(),
            account.clone(),
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            servers_with_accounts,
            app_window.clone(),
        );
        let hooks = window.test_hooks();
        app_window.set_content(Some(&window.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));
        let home_root = hooks.stack.child_by_name("home").expect("home tab exists");
        pump_until(|| find_card_button(&home_root).is_some(), std::time::Duration::from_secs(10));

        let mut measured: Vec<(String, i32)> = Vec::new();
        let mut measure = |name: &str, widget: &gtk4::Widget| {
            pump_until(|| false, std::time::Duration::from_millis(500));
            let (minimum, ..) = widget.measure(gtk4::Orientation::Vertical, 360);
            eprintln!("min height at 360px: {minimum:>4}  {name}");
            measured.push((name.to_string(), minimum));
        };
        let play = |item_id: &str, title: &str| {
            window.controller.start(
                abs_core::auth::Session::new(pool.clone(), &server, &account),
                PlayRequest { item_id: item_id.to_string(), title: title.to_string(), author: Some(LONG_AUTHOR.to_string()) },
                1.0,
            );
        };
        let open_player_and_measure = |name: &str, measure: &mut dyn FnMut(&str, &gtk4::Widget)| {
            (hooks.open_player)();
            pump_until(|| app_window.content().is_some_and(|c| c != window.root), std::time::Duration::from_secs(10));
            measure(name, &app_window.content().unwrap());
            // Nothing scrolls sideways, so a row wider than the phone (the five-button transport
            // row is the widest) would widen the whole window.
            let (min_width, ..) = app_window.content().unwrap().measure(gtk4::Orientation::Horizontal, -1);
            assert!(min_width <= 360, "{name}: the player needs {min_width}px of width, wider than a 360px phone");
            let collapse = find_button_with_icon(&app_window.content().unwrap(), "go-down-symbolic").expect("the player's collapse button");
            collapse.emit_clicked();
            pump_until(|| app_window.content().is_some_and(|c| c == window.root), std::time::Duration::from_secs(5));
        };

        // The shell, on every tab, with a long title in the mini bar.
        play("item-2", LONG_TITLE);
        pump_until(|| window.controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        for tab in ["home", "library", "downloads", "settings"] {
            hooks.stack.set_visible_child_name(tab);
            measure(&format!("shell, {tab} tab, mini bar shown"), &window.root);
        }

        // The full player: short title, long title, long title with an error banner.
        open_player_and_measure("player, long title + author", &mut measure);
        play("item-3", "Piranesi");
        pump_until(|| window.controller.snapshot().is_some_and(|s| s.is_playing), std::time::Duration::from_secs(10));
        open_player_and_measure("player, short title", &mut measure);
        play("item-1", LONG_TITLE);
        pump_until(|| window.controller.snapshot().is_some_and(|s| s.last_error.is_some()), std::time::Duration::from_secs(10));
        open_player_and_measure("player, long title + error banner", &mut measure);

        // A title and author taken from a folder name have no spaces to break at.
        window.controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest {
                item_id: "item-3".to_string(),
                title: "Andrzej.Pilipiuk.Wojslawicka.masakra.kosa.lancuch.gyky".to_string(),
                author: Some("Jan.Kowalski-Nowakowski.Grzegorz_Brzeczyszczykiewicz.Chrzaszczyrzewoszyce".to_string()),
            },
            1.0,
        );
        pump_until(|| window.controller.snapshot().is_some_and(|s| s.is_playing && s.title.starts_with("Andrzej.")), std::time::Duration::from_secs(10));
        open_player_and_measure("player, unspaced title + author", &mut measure);

        // Item Detail (long title and author), About, Welcome.
        hooks.stack.set_visible_child_name("home");
        pump_until(|| false, std::time::Duration::from_millis(300));
        find_card_button(&home_root).expect("a card").emit_clicked();
        pump_until(|| app_window.content().is_some_and(|c| c != window.root), std::time::Duration::from_secs(5));
        measure("item detail", &app_window.content().unwrap());
        app_window.set_content(Some(&window.root));
        crate::screens::settings::push_about(&app_window);
        pump_until(|| app_window.content().is_some_and(|c| c != window.root), std::time::Duration::from_secs(5));
        measure("about", &app_window.content().unwrap());
        let welcome = crate::screens::welcome::build(pool.clone(), crate::test_support::test_paths(), None, |_| {}, None);
        app_window.set_content(Some(&welcome.root));
        measure("welcome", &welcome.root);

        let too_tall: Vec<_> = measured.iter().filter(|(_, minimum)| *minimum > PHONE_MAX_MIN_HEIGHT).collect();
        assert!(too_tall.is_empty(), "screens that can't get as short as a phone screen ({PHONE_MAX_MIN_HEIGHT}px): {too_tall:?}");
    }

    /// A URL nothing listens on: the server is down, and connecting is refused at once.
    fn down_server_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        url
    }

    /// The first label under `root` reading exactly `text` that is actually on screen.
    fn shown_label(root: &gtk4::Widget, text: &str) -> Option<gtk4::Label> {
        if let Some(label) = root.downcast_ref::<gtk4::Label>() {
            if label.text() == text && label.is_mapped() {
                return Some(label.clone());
            }
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(found) = shown_label(&widget, text) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }

    /// The whole shell for `server`/`account` in a phone-sized window, shown.
    fn phone_window(
        runtime: &tokio::runtime::Runtime,
        pool: &SqlitePool,
        server: &abs_storage::models::Server,
        account: &abs_storage::models::Account,
    ) -> (adw::ApplicationWindow, MainWindow) {
        runtime.block_on(abs_storage::repo::accounts::set_active(pool, &account.id)).unwrap();
        let account = &runtime.block_on(abs_storage::repo::accounts::get(pool, &account.id)).unwrap();
        let app_window = adw::ApplicationWindow::builder().default_width(360).default_height(648).build();
        let window = build(
            pool.clone(),
            crate::test_support::test_paths(),
            server.clone(),
            account.clone(),
            abs_core::auth::Session::new(pool.clone(), server, account),
            abs_core::settings::PlaybackSettings::default(),
            abs_core::settings::Theme::default(),
            vec![(server.clone(), vec![account.clone()])],
            app_window.clone(),
        );
        app_window.set_content(Some(&window.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), std::time::Duration::from_secs(5));
        (app_window, window)
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The server down at launch, with
    /// books saved from before: Home and Library both say so — only one of them runs the shared
    /// startup sync, and the other used to redraw from its cache without a word — and the
    /// banner's Retry runs a sync.
    pub(crate) fn run_server_down_at_launch_is_shown_on_home_and_library(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &down_server_url()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Saved Book"));
        let (_app_window, window) = phone_window(runtime, &pool, &server, &account);
        let hooks = window.test_hooks();
        let home = hooks.stack.child_by_name("home").unwrap();
        let library = hooks.stack.child_by_name("library").unwrap();

        pump_until(|| shown_label(&home, "Can't reach your server").is_some(), std::time::Duration::from_secs(10));
        assert!(shown_label(&home, "Can't reach your server").is_some(), "Home must say the server can't be reached");
        assert!(shown_label(&home, "Showing what's saved on this device.").is_some());
        assert!(shown_label(&home, "Saved Book").is_some(), "the saved books stay on screen");

        hooks.stack.set_visible_child_name("library");
        pump_until(|| shown_label(&library, "Can't reach your server").is_some(), std::time::Duration::from_secs(10));
        assert!(shown_label(&library, "Can't reach your server").is_some(), "Library must say it too");

        // Retry runs a sync — which fails again, and says so.
        hooks.stack.set_visible_child_name("home");
        pump_until(|| false, std::time::Duration::from_millis(300));
        let retry = crate::test_support::find_button_with_label(&home, "Retry").expect("the banner offers Retry");
        assert!(retry.is_mapped());
        retry.emit_clicked();
        pump_until(|| crate::test_support::any_label_reads(&home, "Sync failed"), std::time::Duration::from_secs(10));
        assert!(crate::test_support::any_label_reads(&home, "Sync failed"), "Retry must run a sync");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A fresh install with the server
    /// down: Home says it can't reach the server (with Try again) instead of spinning on
    /// "Syncing your libraries…" for good.
    pub(crate) fn run_server_down_on_a_fresh_install_is_shown_on_home(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &down_server_url()));
        let (_app_window, window) = phone_window(runtime, &pool, &server, &account);
        let home = window.test_hooks().stack.child_by_name("home").unwrap();

        pump_until(|| shown_label(&home, "Can't reach your server").is_some(), std::time::Duration::from_secs(10));
        assert!(shown_label(&home, "Can't reach your server").is_some(), "Home must say the server can't be reached");
        assert!(shown_label(&home, "Syncing your libraries…").is_none(), "and stop saying it's syncing");
        assert!(crate::test_support::find_button_with_label(&home, "Try again").is_some_and(|b| b.is_mapped()));
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Play pressed with the server down:
    /// the player says the book can't start (not "lost the connection while playing"), offers
    /// Retry, shows no "0:00 / -0:00", and the toast fits.
    pub(crate) fn run_play_with_the_server_down_says_it_cannot_start(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &down_server_url()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Saved Book"));
        let (app_window, window) = phone_window(runtime, &pool, &server, &account);
        let hooks = window.test_hooks();

        window.controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Saved Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| window.controller.snapshot().is_some_and(|s| s.last_error.is_some()), std::time::Duration::from_secs(15));
        assert_eq!(window.controller.snapshot().unwrap().last_error.map(|e| e.kind), Some(abs_player::PlaybackErrorKind::Unreachable));
        pump_until(|| shown_label(hooks.toast_overlay.upcast_ref(), "Can't reach server").is_some(), std::time::Duration::from_secs(5));
        assert!(shown_label(hooks.toast_overlay.upcast_ref(), "Can't reach server").is_some(), "the toast says it in a few words");

        (hooks.open_player)();
        pump_until(|| app_window.content().is_some_and(|c| c != window.root), std::time::Duration::from_secs(5));
        let player = app_window.content().unwrap();
        pump_until(|| shown_label(&player, "Can't reach your server").is_some(), std::time::Duration::from_secs(5));
        assert!(shown_label(&player, "Can't reach your server").is_some());
        assert!(shown_label(&player, "This book can't start until it's back. Check your connection and try again.").is_some());
        assert!(crate::test_support::find_button_with_label(&player, "Retry").is_some_and(|b| b.is_mapped()), "Retry is offered");
        assert!(shown_label(&player, "Lost the connection while playing — check your connection and try again.").is_none());
        let remaining = shown_label(&player, "-0:00").expect("the remaining-time label");
        assert_eq!(remaining.opacity(), 0.0, "no time shown for a book whose length isn't known");
        window.controller.stop();
    }
}
