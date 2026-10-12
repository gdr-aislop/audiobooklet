//! The GTK application shell. Decides whether to show the Welcome/login screen or the main
//! post-login window (`screens::main_window`) based on whether an account is active, and hosts
//! the single `AdwApplicationWindow` both get swapped into via `set_content`.

use abs_storage::paths::APP_ID;
use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use crate::i18n::tr;
use crate::screens;

thread_local! {
    /// The app's currently-built `MainWindow`, retained for as long as it's the shell actually
    /// shown — the counterpart to every function below that builds one (`show_main`,
    /// `show_main_or_welcome`) or replaces it with the Welcome/login screen (`show_welcome`,
    /// `show_add_server`). Without this, a builder's `MainWindow` was just a bare local variable
    /// dropped the instant its root widget was swapped in — and dropping it ran the `Drop`
    /// impls on its watcher fields (`ModemManagerCallWatcher`, `NetworkManagerConnectivityWatcher`,
    /// and `PulseRouteWatcher`, which unsubscribe/stop on drop), silently killing call-pause,
    /// reconnect-triggered sync and headphone-unplug pause the moment the shell was rebuilt even
    /// once (sign-out/sign-in, switching server or account) — despite those watchers' own "kept
    /// alive for the app's whole lifetime" doc comments, which this makes true.
    ///
    /// A `thread_local` (rather than a field threaded through every `screens::*::build` call) is
    /// deliberate: the app has exactly one `AdwApplicationWindow` for its whole life, bound to
    /// the one GLib main-loop thread everything here already assumes, so there is only ever one
    /// slot to hold, and nothing outside this file needs a handle to it — every replacement of
    /// the shell's content already funnels through the handful of functions below.
    static MAIN_WINDOW: std::cell::RefCell<Option<screens::main_window::MainWindow>> = const { std::cell::RefCell::new(None) };
}

/// Retires whatever `MainWindow` is currently retained (see the `MAIN_WINDOW` doc comment) and
/// retains `new` in its place.
fn retain_main_window(new: screens::main_window::MainWindow) {
    MAIN_WINDOW.with(|slot| *slot.borrow_mut() = Some(new));
}

/// Retires the currently retained `MainWindow`, if any — for wherever the shell's content stops
/// being a `MainWindow` at all (the Welcome/login screen).
fn release_main_window() {
    MAIN_WINDOW.with(|slot| *slot.borrow_mut() = None);
}

/// Removes and returns the currently retained `MainWindow` without dropping it — for
/// `show_add_server`'s Cancel path, which restores the exact previous shell rather than
/// rebuilding one, and so must hand its watchers back too rather than losing them.
fn take_main_window() -> Option<screens::main_window::MainWindow> {
    MAIN_WINDOW.with(|slot| slot.borrow_mut().take())
}

/// Everything the UI needs a handle to. Constructed once in `main` after the async setup step
/// (DB connect + migrate, active-account lookup) completes, then moved into the
/// `connect_activate` closure.
pub struct AppState {
    pub pool: SqlitePool,
    pub paths: abs_storage::AppPaths,
    pub active_account: Option<abs_storage::models::Account>,
    pub playback_settings: abs_core::settings::PlaybackSettings,
    pub theme: abs_core::settings::Theme,
}

/// The user-facing version string, including the short git commit it was built from. The
/// version number is sourced at compile time from this crate's manifest, which itself inherits
/// the workspace's single `[workspace.package].version` — there is exactly one place a version
/// number is written; the commit hash comes from `build.rs`'s `ABS_APP_GIT_HASH` (`"unknown"`
/// for a non-git build, e.g. a distro source tarball). Shared by the `--version`/`-v`
/// command-line flags (handled in `main` before any setup), main's startup diagnostics log line,
/// and, eventually, the Settings screen's About row (docs/design/ui-spec.md) — one line a user
/// can paste into a bug report that identifies exactly which build they're running.
/// The app's display name — window title, version line, About page, media widgets.
pub const APP_NAME: &str = "Audiobooklet";

pub fn version_line() -> String {
    format!("{APP_NAME} {} ({})", env!("CARGO_PKG_VERSION"), env!("ABS_APP_GIT_HASH"))
}

/// Applies a Theme to libadwaita's global style manager — the one place that knows the
/// setting-to-scheme mapping. Called once at startup (before the first window builds) and live
/// from the Settings screen's Theme row.
pub fn apply_theme(theme: abs_core::settings::Theme) {
    let scheme = match theme {
        abs_core::settings::Theme::System => adw::ColorScheme::Default,
        abs_core::settings::Theme::Light => adw::ColorScheme::ForceLight,
        abs_core::settings::Theme::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

pub fn build_application(state: AppState) -> adw::Application {
    let app = adw::Application::builder().application_id(APP_ID).build();

    // A second launch while the app runs only activates this copy again. It brings the existing
    // window forward rather than opening another one: a second window would hold a second login
    // session for the same account, and the two would keep invalidating each other's rotating
    // refresh token.
    app.connect_activate(move |app| {
        if present_existing_window(app) {
            return;
        }
        build_window(app, &state);
        crate::perf::start_main_loop_watchdog();
    });

    quit_cleanly_on_termination_signals(&app);

    app
}

/// SIGTERM (a service manager or `kill`) and SIGINT (Ctrl+C in a terminal) quit the way Ctrl+Q
/// does, so the player's shutdown flush saves the listening position — they used to kill the
/// process outright, losing up to the last few seconds of it.
fn quit_cleanly_on_termination_signals(app: &adw::Application) {
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    for signal in [SIGINT, SIGTERM] {
        let app = app.clone();
        glib::unix_signal_add_local(signal, move || {
            tracing::info!(signal, "asked to terminate; quitting");
            app.quit();
            glib::ControlFlow::Break
        });
    }
}

/// Presents the app's window, if it has one; returns whether it did.
pub(crate) fn present_existing_window(app: &adw::Application) -> bool {
    match app.windows().into_iter().find(|window| window.is::<adw::ApplicationWindow>()) {
        Some(window) => {
            tracing::info!("activated again; showing the open window instead of opening another");
            window.present();
            true
        }
        None => false,
    }
}

fn build_window(app: &adw::Application, state: &AppState) {
    // Before any window content builds — the theme is a global, so the Welcome screen (and every
    // sheet/popover) gets it too, not just the signed-in shell.
    apply_theme(state.theme);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(APP_NAME)
        .default_width(390)
        .default_height(760)
        .build();

    crate::icons::register(&WidgetExt::display(&window));
    add_keyboard_support(app, &window);

    match &state.active_account {
        None => show_welcome(&window, state.pool.clone(), state.paths.clone(), state.playback_settings, None),
        Some(_) => show_main(&window, state.pool.clone(), state.paths.clone(), state.playback_settings),
    }

    window.present();
}

/// Swaps the window's content for the login screen. `seed` turns this into the re-login flow:
/// the form comes pre-filled with the seeded session's URL and username (only the password left
/// to type), a Cancel affordance returns to the main window, and replacing the seeded session's
/// local data with different credentials asks for confirmation first. `None` is the plain
/// first-run flow.
pub(crate) fn show_welcome(
    window: &adw::ApplicationWindow,
    pool: SqlitePool,
    paths: abs_storage::AppPaths,
    playback_settings: abs_core::settings::PlaybackSettings,
    seed: Option<abs_core::accounts::ReloginSeed>,
) {
    let window_for_callback = window.clone();
    let on_success_pool = pool.clone();
    let on_success_paths = paths.clone();
    // Cancel only exists in the re-login flow (first run has nothing to go back to), and goes
    // back to the main window resolved fresh from the database — the session it returns to is
    // exactly the seeded one, untouched by an aborted re-login.
    let on_cancel: Option<std::rc::Rc<dyn Fn()>> = seed.as_ref().map(|_| {
        let pool = pool.clone();
        let paths = paths.clone();
        let window = window.clone();
        std::rc::Rc::new(move || show_main(&window, pool.clone(), paths.clone(), playback_settings))
            as std::rc::Rc<dyn Fn()>
    });

    let screen = screens::welcome::build(
        pool,
        paths,
        seed,
        move |_added| {
            // The just-logged-in account is the active one by the time on_success fires, so
            // resolving the main window from the DB's active account (rather than the callback's
            // ids) builds the identical shell — and doubles as the Cancel flow's entry point.
            show_main(&window_for_callback, on_success_pool.clone(), on_success_paths.clone(), playback_settings);
        },
        on_cancel,
    );
    // Whatever `MainWindow` was retained (there may be none, on the plain first-run flow) stops
    // being the shown shell here — see the `MAIN_WINDOW` doc comment. The re-login Cancel path
    // above deliberately does *not* get its watchers back this way: it rebuilds via `show_main`
    // from the database rather than restoring the exact prior widget, so a fresh `MainWindow`
    // (and fresh watchers) is what it gets instead, same as every other session change.
    release_main_window();
    crate::widgets::swap_content(window, &screen.root);
}

/// "Add Server" from Settings' Servers group: the plain first-run login flow (`previous=None`
/// runs `add_server_and_login`), shown over the signed-in shell with a Cancel affordance back
/// to it — the Welcome screen only drops its Cancel button when no way back exists, so one is
/// provided here. A successful connect makes the new account the active one and rebuilds the
/// shell from the database exactly like every other session change.
pub(crate) fn show_add_server(
    window: &adw::ApplicationWindow,
    pool: SqlitePool,
    paths: abs_storage::AppPaths,
    playback_settings: abs_core::settings::PlaybackSettings,
) {
    // Captured before the swap: Cancel restores this exact widget, so the shell keeps its state
    // (open tab, scroll positions) — the same policy as collapsing the full player. The
    // currently retained `MainWindow` (with its watchers) is captured the same way and for the
    // same reason: Cancel below hands it right back rather than rebuilding, so its watchers
    // (call-pause, reconnect sync, headphone-unplug pause) are exactly as continuous across the
    // detour as the widget itself. A successful connect, by contrast, rebuilds: the new account
    // is the active one and the old shell's session is stale, so `show_main` retains a fresh one.
    let previous_main_window = std::rc::Rc::new(std::cell::RefCell::new(take_main_window()));
    let on_cancel: std::rc::Rc<dyn Fn()> = match window.content() {
        Some(previous_root) => {
            let window = window.clone();
            let previous_main_window = previous_main_window.clone();
            std::rc::Rc::new(move || {
                if let Some(main_window) = previous_main_window.borrow_mut().take() {
                    retain_main_window(main_window);
                }
                crate::widgets::swap_content(&window, &previous_root);
            })
        }
        None => {
            let pool = pool.clone();
            let paths = paths.clone();
            let window = window.clone();
            std::rc::Rc::new(move || show_main(&window, pool.clone(), paths.clone(), playback_settings))
        }
    };
    let on_success_pool = pool.clone();
    let on_success_paths = paths.clone();
    let on_success_window = window.clone();
    let screen = screens::welcome::build(
        pool,
        paths,
        None,
        move |_added| {
            // The parked shell's session is stale now: retire it (saving its position, releasing
            // its MPRIS registration) before the new shell is built, not whenever the Welcome
            // screen happens to be finalized.
            drop(previous_main_window.borrow_mut().take());
            show_main(&on_success_window, on_success_pool.clone(), on_success_paths.clone(), playback_settings)
        },
        Some(on_cancel),
    );
    crate::widgets::swap_content(window, &screen.root);
}

/// The single entry point after any session mutation from Settings (switch server, sign out,
/// remove server): resolves the database's active account and swaps the window's content for
/// the main shell when one exists, or the plain first-run Welcome screen when none does (the
/// last account signed out or the last server removed). The old shell — and with it any
/// ongoing playback — is dropped by the swap; ending the session that was playing is the
/// point of signing out, not a side effect to engineer around.
pub(crate) fn show_main_or_welcome(
    window: &adw::ApplicationWindow,
    pool: SqlitePool,
    paths: abs_storage::AppPaths,
    playback_settings: abs_core::settings::PlaybackSettings,
) {
    show_main_or_welcome_on(window, pool, paths, playback_settings, None);
}

/// [`show_main_or_welcome`], opening the main shell on `tab` (a page name such as `"settings"`)
/// instead of Home — for a Settings action that rebuilds the shell but leaves the user where they
/// were (signing out of, or removing, a server that isn't the active one).
pub(crate) fn show_main_or_welcome_on(
    window: &adw::ApplicationWindow,
    pool: SqlitePool,
    paths: abs_storage::AppPaths,
    playback_settings: abs_core::settings::PlaybackSettings,
    tab: Option<&'static str>,
) {
    let window_for_callback = window.clone();
    glib::spawn_future_local(async move {
        let active = abs_storage::repo::accounts::get_active(&pool)
            .await
            .expect("checking for an active account must not fail");
        match active {
            Some(_) => {
                // Before the new shell exists, not after: its MPRIS registration would collide
                // with the old shell's, which keeps the object path until it is retired.
                release_main_window();
                let main_window =
                    build_main_window(pool, paths, playback_settings, window_for_callback.clone())
                        .await;
                crate::widgets::swap_content(&window_for_callback, &main_window.root);
                retain_main_window(main_window);
                if let Some(tab) = tab {
                    use gtk4::prelude::ToVariant;
                    let _ = WidgetExt::activate_action(&window_for_callback, "win.switch-tab", Some(&tab.to_variant()));
                }
            }
            None => show_welcome(&window_for_callback, pool, paths, playback_settings, None),
        }
    });
}

/// Swaps the window's content for the main shell, resolved from the database's active account —
/// the single entry point for every path back into the app (startup, post-login, re-login
/// cancel).
pub(crate) fn show_main(
    window: &adw::ApplicationWindow,
    pool: SqlitePool,
    paths: abs_storage::AppPaths,
    playback_settings: abs_core::settings::PlaybackSettings,
) {
    let window_for_content = window.clone();
    glib::spawn_future_local(async move {
        // See `show_main_or_welcome_on`: the old shell must be retired before the new one registers MPRIS.
        release_main_window();
        let main_window = build_main_window(pool, paths, playback_settings, window_for_content.clone()).await;
        crate::widgets::swap_content(&window_for_content, &main_window.root);
        retain_main_window(main_window);
    });
}

/// Resolves the active account (and its server) from storage and builds the main shell. Panics
/// on a missing active account — every caller guarantees one by construction (checked at startup,
/// or just created by a successful login), so this is a caller bug, not a runtime condition.
async fn build_main_window(
    pool: SqlitePool,
    paths: abs_storage::AppPaths,
    playback_settings: abs_core::settings::PlaybackSettings,
    window: adw::ApplicationWindow,
) -> screens::main_window::MainWindow {
    // Re-fetch the full rows rather than threading server/account fields through callbacks —
    // ids are all the login flows hand back.
    let account = abs_storage::repo::accounts::get_active(&pool)
        .await
        .expect("checking for an active account must not fail")
        .expect("the main window requires an active account");
    let server = abs_storage::repo::servers::get(&pool, &account.server_id)
        .await
        .expect("an active account's server must exist");
    let theme = abs_core::settings::load_theme(&pool).await.expect("load theme");
    // The copy threaded through the callbacks dates from startup; settings saved since then live
    // only in the database, and a stale copy would be written back by the next edit.
    let playback_settings = abs_core::settings::load_playback_settings(&pool).await.unwrap_or(playback_settings);
    let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
    session.set_keep_awake(std::sync::Arc::new(crate::keep_awake::GtkKeepAwake));
    session.set_password_store(crate::password_keyring::store());
    // Before any screen is built: the first sync, cover fetch or progress push must already see
    // offline mode if it is on (the shell's `OfflineModeState` only loads it asynchronously).
    let offline_mode = abs_core::settings::load_offline_mode(&pool).await.unwrap_or(false);
    session.set_offline(offline_mode);
    tracing::info!(offline_mode, "loaded the offline mode setting");
    let servers_with_accounts = fetch_servers_with_accounts(&pool).await;
    screens::main_window::build(
        pool,
        paths,
        server,
        account,
        session,
        playback_settings,
        theme,
        servers_with_accounts,
        window,
    )
}

/// Every configured server with its accounts, in creation order — the data behind Settings'
/// Account and Servers groups. Each server's account list is fetched separately (a join would
/// hand back flat rows to regroup anyway).
pub(crate) async fn fetch_servers_with_accounts(pool: &SqlitePool) -> Vec<(abs_storage::models::Server, Vec<abs_storage::models::Account>)> {
    let servers = abs_storage::repo::servers::list(pool).await.expect("listing servers must not fail");
    let mut result = Vec::with_capacity(servers.len());
    for server in servers {
        let accounts = abs_storage::repo::accounts::list_for_server(pool, &server.id)
            .await
            .expect("listing a server's accounts must not fail");
        result.push((server, accounts));
    }
    result
}

/// Accelerators registered app-wide through `set_accels_for_action`. Every one of them **must**
/// carry a modifier: GTK4 runs application accelerators in the *capture* phase at the window,
/// ahead of the focused widget, so a bare key here (`b`, `space`, ...) would be consumed before a
/// focused text entry could type it. Modifier-less bindings live in `SINGLE_KEY_SHORTCUTS`.
const APP_ACCELS: &[(&str, &[&str])] = &[
    ("app.quit", &["<Control>q"]),
    // GtkApplicationWindow also wires this action itself; stating the accel keeps the
    // binding visible and independent of that internal default.
    ("win.show-help-overlay", &["<Control>question"]),
    ("win.switch-tab('home')", &["<Alt>1"]),
    ("win.switch-tab('library')", &["<Alt>2"]),
    ("win.switch-tab('downloads')", &["<Alt>3"]),
    ("win.switch-tab('settings')", &["<Alt>4", "<Control>comma"]),
    ("win.open-library-search", &["<Control>f"]),
    // Dual bindings (with and without Ctrl) match Decibels, GNOME's own audio player and the
    // closest analogue for these controls; the bare halves are in `SINGLE_KEY_SHORTCUTS`.
    ("player.speed-up", &["<Control>plus"]),
    ("player.speed-down", &["<Control>minus"]),
    ("player.speed-reset", &["<Control>0"]),
];

/// Modifier-less `(trigger, action)` bindings, installed by `add_single_key_shortcuts`.
const SINGLE_KEY_SHORTCUTS: &[(&str, &str)] = &[
    ("space", "win.play-pause"),
    ("b", "win.bookmark"),
    ("Left", "player.skip-back"),
    ("Right", "player.skip-forward"),
    ("plus", "player.speed-up"),
    ("minus", "player.speed-down"),
    ("0", "player.speed-reset"),
    ("c", "player.chapters"),
    ("t", "player.sleep-timer"),
    ("Escape", "player.collapse"),
];

/// Installs the modifier-less keyboard shortcuts on `widget` (the application window) so that
/// "typing always wins" (ui-spec §6): the controller is in the *bubble* phase, so the focused
/// widget — a `GtkText` inside the Library search, the login form, ... — gets first refusal on
/// every key and the shortcut only fires if nothing focused wanted it. `Global` scope keeps it
/// working wherever focus sits inside the window. Actions are referenced by name, so they resolve
/// lazily: `win.*` ones don't exist before login and `player.*` ones are inert until the full
/// player merges its action group.
pub(crate) fn add_single_key_shortcuts(widget: &impl IsA<gtk4::Widget>) -> gtk4::ShortcutController {
    let controller = gtk4::ShortcutController::new();
    controller.set_scope(gtk4::ShortcutScope::Global);
    controller.set_propagation_phase(gtk4::PropagationPhase::Bubble);
    for (trigger, action) in SINGLE_KEY_SHORTCUTS {
        let trigger = gtk4::ShortcutTrigger::parse_string(trigger)
            .unwrap_or_else(|| panic!("invalid shortcut trigger {trigger:?}"));
        controller.add_shortcut(gtk4::Shortcut::new(Some(trigger), Some(gtk4::NamedAction::new(action))));
    }
    widget.add_controller(controller.clone());
    controller
}

/// The keyboard half of ui-spec §6: the modifier accelerators (app-wide — they only fire when
/// the matching action actually exists, so `player.*` accels are inert until the full player
/// merges its action group, and `win.*` ones don't exist before login), the modifier-less
/// shortcuts (see `add_single_key_shortcuts`) and the `Ctrl+?` shortcuts overlay. The actions
/// themselves live where their state is: `win.*` in `screens::main_window`, `player.*` on the
/// full player screen.
fn add_keyboard_support(app: &adw::Application, window: &adw::ApplicationWindow) {
    let quit_action = adw::gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| app.quit()
    });
    app.add_action(&quit_action);

    window.set_help_overlay(Some(&build_shortcuts_overlay()));

    for (action, accels) in APP_ACCELS {
        app.set_accels_for_action(action, accels);
    }
    add_single_key_shortcuts(window);
}

/// One `GtkShortcutsGroup` block of the overlay's `GtkBuilder` definition: `title` plus a
/// `(title, accelerator)` row per shortcut. Titles are escaped here; accelerators are already
/// XML-escaped literals.
fn shortcuts_group(title: &str, shortcuts: &[(&str, &str)]) -> String {
    let mut group = format!(
        "        <child>\n          <object class=\"GtkShortcutsGroup\">\n            <property name=\"title\">{}</property>\n",
        xml_escape(title)
    );
    for (shortcut_title, accelerator) in shortcuts {
        group.push_str(&format!(
            "            <child>\n              <object class=\"GtkShortcutsShortcut\">\n                <property name=\"title\">{}</property>\n                <property name=\"accelerator\">{accelerator}</property>\n              </object>\n            </child>\n",
            xml_escape(shortcut_title)
        ));
    }
    group.push_str("          </object>\n        </child>\n");
    group
}

/// Escapes text for use inside an XML element.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The `GtkShortcutsWindow` behind `Ctrl+?` — one item per row of ui-spec §6's table, grouped
/// the same way, except that two of that table's rows ("Skip back / forward", "Speed up /
/// down") become *two* rows here. `GtkShortcutsShortcut:accelerator` parses as one or two real
/// accelerators for a single action (space-separated, each run through
/// `gtk_accelerator_parse()`), not a human "A / B" display string for two different actions —
/// using "/" as a delimiter there logs a `Gtk-WARNING` ("Failed to parse /") and was never
/// actually valid, so each pair of distinct actions gets its own row instead. "Go to Settings"
/// keeps one row: its two accelerators really are alternates for the same action, so they're
/// just space- rather than slash-separated.
///
/// Built from an inline `GtkBuilder` definition rather than the Rust widget bindings: the
/// `ShortcutsWindow` family's `add_section`/`add_group`/`add_shortcut` methods are gated behind
/// gtk4-rs's `v4_14` feature, and this crate deliberately pins `v4_8` as its API ceiling (see
/// `app/Cargo.toml` — loosening it for three methods would let future code reach real 4.14-only
/// APIs by accident). The underlying C symbols are ancient (GTK 4.0), so the runtime library on
/// PureOS Crimson handles this fine; a declarative definition is also the form upstream apps
/// themselves use for this widget (Decibels ships its shortcuts dialog as a `.ui` file).
fn build_shortcuts_overlay() -> gtk4::ShortcutsWindow {
    // Titles go through `tr` here, at the call sites, because `xgettext` can't see text inside an
    // XML definition; `shortcuts_group` escapes them before they are spliced into the markup.
    let definition = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<interface>
  <object class="GtkShortcutsWindow" id="overlay">
    <child>
      <object class="GtkShortcutsSection">
{general}{navigation}{playback}{full_player}      </object>
    </child>
  </object>
</interface>
"#,
        general = shortcuts_group(
            &tr("General"),
            &[
                (&tr("Show shortcuts"), "&lt;Control&gt;question"),
                (&tr("Quit"), "&lt;Control&gt;q"),
            ],
        ),
        navigation = shortcuts_group(
            &tr("Navigation"),
            &[
                (&tr("Go to Home"), "&lt;Alt&gt;1"),
                (&tr("Go to Library"), "&lt;Alt&gt;2"),
                (&tr("Go to Downloads"), "&lt;Alt&gt;3"),
                (&tr("Go to Settings"), "&lt;Alt&gt;4 &lt;Control&gt;comma"),
                (&tr("Search the library"), "&lt;Control&gt;f"),
            ],
        ),
        playback = shortcuts_group(
            &tr("Playback"),
            &[
                (&tr("Play / pause"), "space"),
                (&tr("Add bookmark"), "b"),
            ],
        ),
        full_player = shortcuts_group(
            &tr("Full player"),
            &[
                (&tr("Skip back"), "Left"),
                (&tr("Skip forward"), "Right"),
                (&tr("Speed up"), "&lt;Control&gt;plus plus"),
                (&tr("Speed down"), "&lt;Control&gt;minus minus"),
                (&tr("Reset speed to 1×"), "&lt;Control&gt;0"),
                (&tr("Open chapters"), "c"),
                (&tr("Open sleep timer"), "t"),
                (&tr("Collapse player"), "Escape"),
            ],
        ),
    );

    let builder = gtk4::Builder::from_string(&definition);
    builder.object::<gtk4::ShortcutsWindow>("overlay").expect("the shortcuts overlay definition must contain its root object")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn version_line_is_the_display_name_plus_a_dotted_version_plus_a_commit() {
        let line = version_line();
        let rest = line
            .strip_prefix("Audiobooklet ")
            .expect("version line should start with the display name");
        let (version, commit) = rest.split_once(" (").expect("version line should have a parenthesized commit after the version");
        let commit = commit.strip_suffix(')').expect("the commit part should be closed with a parenthesis");
        assert!(!commit.is_empty(), "commit must not be empty (should be at least \"unknown\")");

        assert!(!version.is_empty(), "version must not be empty");
        let parts: Vec<&str> = version.split('.').collect();
        assert!(parts.len() >= 2, "version should be dotted, got: {version}");
        assert!(
            parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            "version should be numeric and dotted, got: {version}"
        );
    }

    /// The capture-phase accelerator table must never carry a bare key: GTK4 runs those ahead of
    /// the focused widget, which is exactly how `b` stopped typing in the Library search box.
    #[test]
    fn app_accels_all_carry_a_modifier() {
        for (action, accels) in APP_ACCELS {
            for accel in *accels {
                assert!(
                    accel.starts_with('<'),
                    "{action}: accelerator {accel:?} has no modifier; bare keys belong in SINGLE_KEY_SHORTCUTS"
                );
            }
        }
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A second activation (the app launched
    /// again while it runs) must bring the open window forward rather than build another, and
    /// only the app's main window counts: an open shortcuts window doesn't stand in for it.
    pub(crate) fn run_activating_again_presents_the_open_window(_runtime: &tokio::runtime::Runtime) {
        let app = adw::Application::new(Some("io.github.gdr_aislop.audiobooklet.Test"), gtk4::gio::ApplicationFlags::NON_UNIQUE);
        app.register(gtk4::gio::Cancellable::NONE).expect("register the test application");
        assert!(!present_existing_window(&app), "nothing to present before the first window");

        let shortcuts = gtk4::Window::builder().application(&app).build();
        assert!(!present_existing_window(&app), "a secondary window isn't the app's window");

        let window = adw::ApplicationWindow::builder().application(&app).build();
        assert!(present_existing_window(&app), "the open window is presented");
        assert_eq!(app.windows().len(), 2, "and no window is added");

        shortcuts.destroy();
        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The modifier-less shortcuts must sit
    /// in a global-scope *bubble*-phase controller (so a focused text entry types first) and cover
    /// exactly `SINGLE_KEY_SHORTCUTS`. Real key events can't be injected in a GTK4 test, so this
    /// pins the configuration that produces the behavior.
    pub(crate) fn run_single_key_shortcuts_defer_to_the_focused_widget(_runtime: &tokio::runtime::Runtime) {
        let window = adw::ApplicationWindow::builder().build();
        let controller = add_single_key_shortcuts(&window);

        assert_eq!(controller.propagation_phase(), gtk4::PropagationPhase::Bubble);
        assert_eq!(controller.scope(), gtk4::ShortcutScope::Global);

        let model = controller.upcast_ref::<gtk4::gio::ListModel>();
        assert_eq!(model.n_items() as usize, SINGLE_KEY_SHORTCUTS.len());
        let mut found = Vec::new();
        for i in 0..model.n_items() {
            let shortcut = model.item(i).unwrap().downcast::<gtk4::Shortcut>().unwrap();
            let trigger = shortcut.trigger().expect("every shortcut has a trigger").to_str().to_string();
            let action = shortcut
                .action()
                .and_then(|a| a.downcast::<gtk4::NamedAction>().ok())
                .expect("every shortcut targets a named action")
                .action_name()
                .to_string();
            found.push((trigger, action));
        }
        let expected: Vec<(String, String)> =
            SINGLE_KEY_SHORTCUTS.iter().map(|(t, a)| (t.to_string(), a.to_string())).collect();
        assert_eq!(found, expected);
    }
}
