//! The Home tab's content — one of `main_window`'s four `AdwViewStack` pages. Shows "Continue
//! Listening"/"Recently Added" shelves and a "Your Libraries" list, backed by data synced via
//! `abs_core::sync::sync_all`. See `docs/design/ui-spec.md`'s "Home" section and the published
//! mockup for the visual design this implements.
//!
//! Manual re-sync ("Sync now", ui-spec Home): a header-bar ⋯ overflow item plus a pull-to-refresh
//! gesture on the main scroller, both feeding the same sync cycle the screen opens with — see
//! `spawn_sync_cycle` and `crate::widgets::ManualSync`. Cover art reuses the same
//! `abs_core::covers`/`CoverImage` pipeline the player screen already built.

use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use abs_core::error::{CoreError, Result as CoreResult};
use abs_storage::models::{Account, Item, Library, Progress, Server};
use abs_storage::AppPaths;

use crate::i18n::{tr, tr_args, tr_ctx};
use crate::player::PlayRequest;
use crate::widgets::item_card;

/// Which shelf's heading was tapped — the shell translates this into a Library navigation with
/// the matching sort (and, for Continue Listening, the in-progress filter). See
/// `docs/design/ui-spec.md`'s Home tap-through behavior.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Shelf {
    RecentlyAdded,
    ContinueListening,
}

pub struct HomeScreen {
    pub root: gtk4::Widget,
    /// What `follow_downloads` needs to re-render the shelves' downloaded badges.
    widgets: HomeWidgets,
    pool: SqlitePool,
    server_id: String,
    account_id: String,
    #[cfg(test)]
    hooks: TestHooks,
}

impl HomeScreen {
    /// Re-renders the shelves when low memory mode changes, so the covers already shown turn into
    /// placeholders (or come back).
    pub(crate) fn follow_low_memory_mode(&self, low_memory_mode: &crate::low_memory_mode::LowMemoryModeState) {
        let widgets = self.widgets.clone();
        low_memory_mode.add_listener(move |_| {
            let widgets = widgets.clone();
            glib::idle_add_local_once(move || {
                let cached = widgets.last_data.borrow().clone();
                if let Some(data) = cached {
                    apply(&data, &widgets);
                }
            });
        });
    }

    /// Re-reads the shelves a moment after playback pauses or stops, so "Continue Listening" shows
    /// the book just listened to without waiting for the next sync (found in the 2026-10-12
    /// pass). Local storage only; the delay lets the player's queued progress write land first.
    pub(crate) fn follow_player(&self, controller: &crate::player::PlayerController) {
        let (pool, server_id, account_id, widgets) = (self.pool.clone(), self.server_id.clone(), self.account_id.clone(), self.widgets.clone());
        let root = self.root.downgrade();
        let was_playing = std::cell::Cell::new(false);
        let pending = std::rc::Rc::new(std::cell::Cell::new(false));
        controller.add_listener(move |snapshot| {
            let stopped = was_playing.replace(snapshot.is_playing) && !snapshot.is_playing;
            if !stopped || pending.get() || root.upgrade().is_none() {
                return;
            }
            pending.set(true);
            let (pool, server_id, account_id, widgets, root, pending) =
                (pool.clone(), server_id.clone(), account_id.clone(), widgets.clone(), root.clone(), pending.clone());
            glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
                pending.set(false);
                if root.upgrade().is_none() {
                    return;
                }
                glib::spawn_future_local(async move {
                    if let Ok(data) = load(&pool, &server_id, &account_id).await {
                        if !data.libraries.is_empty() {
                            apply(&data, &widgets);
                        }
                    }
                });
            });
        });
    }

    /// Keeps the shelves' downloaded badges (and the offline-mode view) current as downloads
    /// finish or are cleared — see `crate::downloads::follow_downloaded_items`.
    pub(crate) fn follow_downloads(&self, download_manager: &crate::downloads::DownloadManager) {
        let widgets = self.widgets.clone();
        crate::downloads::follow_downloaded_items(download_manager, self.pool.clone(), self.server_id.clone(), self.root.downgrade(), move |downloaded| {
            let data = {
                let mut last = widgets.last_data.borrow_mut();
                let Some(data) = last.as_mut() else { return };
                if data.downloaded == downloaded {
                    return;
                }
                tracing::debug!(downloaded = downloaded.len(), "downloaded books changed; refreshing Home's badges");
                data.downloaded = downloaded;
                data.clone()
            };
            apply(&data, &widgets);
        });
    }
}

#[cfg(test)]
pub struct TestHooks {
    pub(crate) empty_state: EmptyState,
    pub libraries_list: gtk4::ListBox,
    pub continue_section: gtk4::Box,
    pub continue_heading: gtk4::Button,
    pub continue_row: gtk4::Box,
    pub recent_row: gtk4::Box,
    pub recent_heading: gtk4::Button,
    pub banner: crate::widgets::banner::ErrorBanner,
    pub offline_toggle: gtk4::ToggleButton,
    pub offline_banner: gtk4::Revealer,
    pub sync_now_button: gtk4::Button,
    pub toast_overlay: adw::ToastOverlay,
    pub scroller: gtk4::ScrolledWindow,
    pub pull_spinner: gtk4::Spinner,
}

#[cfg(test)]
impl HomeScreen {
    pub fn test_hooks(&self) -> &TestHooks {
        &self.hooks
    }
}

/// The full-screen view Home shows while it has no local data to render, standing in for the
/// `AdwStatusPage` the screen launched with. That page couldn't host a spinner or a button, so a
/// fresh login sat on a static "No library synced yet" dead end while the sync ran invisibly in
/// the background — and a failed first sync couldn't even surface its error, because the banner
/// lived inside the (then-hidden) scroller. This widget instead has one mode per sync-lifecycle
/// state (see ui-spec.md's "Home" section): syncing, failed (with retry), genuinely-empty.
///
/// Fields are open to the same module (the tests read them directly); everything is a
/// reference-counted handle, so the whole struct is cheap to clone.
#[derive(Clone)]
pub(crate) struct EmptyState {
    root: gtk4::Box,
    spinner: gtk4::Spinner,
    icon: gtk4::Image,
    title: gtk4::Label,
    description: gtk4::Label,
    details: gtk4::Label,
    /// "Details" — the raw error stays one tap away rather than filling the page.
    details_toggle: gtk4::ToggleButton,
    retry: gtk4::Button,
    login_again: gtk4::Button,
    buttons: gtk4::Box,
}

impl EmptyState {
    /// Constructs in the syncing state — the right default, since the only way Home starts with
    /// no visible data is a sync that hasn't landed yet.
    fn build() -> Self {
        let spinner = gtk4::Spinner::builder().spinning(true).visible(false).build();
        let icon = gtk4::Image::builder().icon_name("folder-music-symbolic").pixel_size(64).css_classes(["dim-label"]).visible(false).build();
        let title = gtk4::Label::builder().css_classes(["title-2"]).build();
        let description = gtk4::Label::builder()
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .justify(gtk4::Justification::Center)
            .css_classes(["dim-label"])
            .build();
        // Raw error text for the failed mode: a self-hosted user debugging TLS/proxy setups gets
        // the real cause (selectable, so it can be copied), same posture as the ErrorBanner's
        // details expander. Hidden unless `show_error` says otherwise. `max_width_chars(1)` caps
        // this label's natural width regardless of `wrap` — this `EmptyState` sits directly in
        // `body` with no horizontal scroller in its ancestry, so an unclamped long error message
        // could otherwise force the window wider than the screen (see `widgets::banner`'s
        // identical fix for the same reasoning).
        let details = gtk4::Label::builder()
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .max_width_chars(1)
            .justify(gtk4::Justification::Center)
            .selectable(true)
            .css_classes(["dim-label", "caption"])
            .visible(false)
            .build();
        crate::widgets::banner::ensure_banner_css();
        let details_toggle =
            gtk4::ToggleButton::builder().label(tr("Details")).css_classes(["app-link"]).halign(gtk4::Align::Center).visible(false).build();
        let details_revealer = gtk4::Revealer::builder().child(&details).reveal_child(false).build();
        details_toggle.connect_toggled({
            let details_revealer = details_revealer.clone();
            move |toggle| {
                details_revealer.set_reveal_child(toggle.is_active());
                toggle.set_label(&if toggle.is_active() { tr("Hide details") } else { tr("Details") });
            }
        });
        let retry = gtk4::Button::builder()
            .label(tr("Try again"))
            .css_classes(["pill", "suggested-action"])
            .visible(false)
            .build();
        // Only revealed by the authorization-failure mode — the one failure "Try again" can't
        // fix, since the stored session itself is what's dead.
        let login_again = gtk4::Button::builder()
            .label(tr("Log in again"))
            .css_classes(["pill"])
            .visible(false)
            .build();

        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(12)
            .valign(gtk4::Align::Center)
            .halign(gtk4::Align::Center)
            .vexpand(true)
            .margin_start(24)
            .margin_end(24)
            .visible(false)
            .build();
        root.append(&spinner);
        root.append(&icon);
        root.append(&title);
        root.append(&description);

        let buttons = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk4::Align::Center)
            .margin_top(6)
            .visible(false)
            .build();
        buttons.append(&retry);
        buttons.append(&login_again);
        root.append(&buttons);
        root.append(&details_toggle);
        root.append(&details_revealer);

        let state = Self { root, spinner, icon, title, description, details, details_toggle, retry, login_again, buttons };
        state.show_syncing();
        state
    }

    fn show_syncing(&self) {
        self.spinner.set_visible(true);
        self.spinner.start();
        self.icon.set_visible(false);
        self.title.set_label(&tr("Syncing your libraries…"));
        self.description.set_label(&tr("This can take a moment on first sync."));
        self.set_details("");
        self.buttons.set_visible(false);
        self.root.set_visible(true);
    }

    fn show_error(&self, err: &CoreError) {
        self.spinner.set_visible(false);
        self.spinner.stop();
        self.icon.set_visible(true);
        if matches!(err, CoreError::Unreachable(_)) {
            self.icon.set_icon_name(Some("network-offline-symbolic"));
            self.title.set_label(&tr("Can't reach your server"));
            self.description.set_label(&tr("Check your connection, or that the server is running, then try again."));
        } else {
            self.icon.set_icon_name(Some("dialog-warning-symbolic"));
            self.title.set_label(&tr("Couldn't sync your libraries"));
            self.description.set_label(&tr("Check your connection and try again."));
        }
        let error = err.to_string();
        self.set_details(&error);
        self.buttons.set_visible(true);
        self.retry.set_visible(true);
        self.login_again.set_visible(false);
        self.root.set_visible(true);
    }

    /// The authorization-failure mode: the session itself is dead (revoked/expired refresh token,
    /// removed or demoted user), so "Try again" would just 401 forever — the way out is signing
    /// in again, which the extra button offers (the shell routes it to a pre-filled login
    /// screen). Both buttons stay up: a retry costs nothing and transient misconfigurations do
    /// happen on self-hosted servers.
    fn show_auth_error(&self, error: &str) {
        self.spinner.set_visible(false);
        self.spinner.stop();
        self.icon.set_visible(true);
        self.icon.set_icon_name(Some("system-lock-screen-symbolic"));
        self.title.set_label(&tr("Sign in again"));
        self.description.set_label(&tr("Your session on this server has expired or was revoked."));
        self.set_details(error);
        self.buttons.set_visible(true);
        self.retry.set_visible(true);
        self.login_again.set_visible(true);
        self.root.set_visible(true);
    }

    /// The sync went fine but this device's own database couldn't be read back. Showing
    /// "No library synced yet" here would claim the server has nothing.
    fn show_local_read_error(&self, error: &str) {
        self.spinner.set_visible(false);
        self.spinner.stop();
        self.icon.set_visible(true);
        self.icon.set_icon_name(Some("dialog-warning-symbolic"));
        self.title.set_label(&tr("Couldn't read local data"));
        self.description.set_label(&tr("The library synced, but the app couldn't read it back. Try again, or restart the app."));
        self.set_details(error);
        self.buttons.set_visible(true);
        self.retry.set_visible(true);
        self.login_again.set_visible(false);
        self.root.set_visible(true);
    }

    fn show_empty(&self) {
        self.spinner.set_visible(false);
        self.spinner.stop();
        self.icon.set_visible(true);
        self.icon.set_icon_name(Some("folder-music-symbolic"));
        self.title.set_label(&tr("No library synced yet"));
        self.description.set_label(&tr("This server doesn't have any libraries yet."));
        self.set_details("");
        self.buttons.set_visible(true);
        self.retry.set_visible(true);
        self.login_again.set_visible(false);
        self.root.set_visible(true);
    }

    /// Offline mode on and nothing stored for this server yet (it was added while offline): the
    /// server may well have libraries — this device just hasn't asked. Found in the 2026-10-12
    /// pass, where this said "This server doesn't have any libraries yet."
    fn show_offline_empty(&self) {
        self.spinner.set_visible(false);
        self.spinner.stop();
        self.icon.set_visible(true);
        self.icon.set_icon_name(Some("airplane-mode-symbolic"));
        self.title.set_label(&tr("Offline mode is on"));
        self.description.set_label(&tr("Turn it off to load this server's libraries."));
        self.set_details("");
        self.buttons.set_visible(false);
        self.root.set_visible(true);
    }

    fn hide(&self) {
        self.root.set_visible(false);
        self.spinner.stop();
    }

    /// The raw error behind "Details", collapsed; `""` hides both.
    fn set_details(&self, error: &str) {
        let has_details = !error.is_empty();
        self.details.set_label(error);
        self.details.set_visible(has_details);
        self.details_toggle.set_active(false);
        self.details_toggle.set_visible(has_details);
    }
}

/// Everything `apply` needs a handle to, cloned as a whole into the `spawn_future_local` block —
/// every field is a reference-counted GTK/Adwaita widget handle, so cloning is cheap and shares
/// the same underlying widgets, not copies.
#[derive(Clone)]
struct HomeWidgets {
    empty_state: EmptyState,
    scroller: gtk4::ScrolledWindow,
    continue_section: gtk4::Box,
    continue_row: gtk4::Box,
    recent_row: gtk4::Box,
    libraries_list: gtk4::ListBox,
    banner: crate::widgets::banner::ErrorBanner,
    on_open: std::rc::Rc<dyn Fn(PlayRequest)>,
    /// Shared, live-updating state with Library's own toggle (ui-spec: "not a per-screen
    /// setting") — see `crate::offline_mode::OfflineModeState`'s doc for why this can't be a
    /// screen-local `Cell` (that was the actual bug: toggling on one screen never reached the
    /// other's already-built instance).
    offline_mode: crate::offline_mode::OfflineModeState,
    /// The last data `apply()` rendered, so the offline-mode toggle can re-filter and re-render
    /// without a full re-sync — same "keep the last render around" shape `library.rs`'s
    /// `LibraryData` cache already uses, just not behind its own repaint-triggering setter here
    /// since Home's `apply()` is already the single re-render entry point.
    last_data: std::rc::Rc<std::cell::RefCell<Option<HomeData>>>,
}

/// Everything one sync cycle needs, cloned as a whole so both the initial run and every
/// "Try again" press can own an independent copy. `server`/`account` are full rows (resolved
/// once by the caller, `main_window`) rather than bare ids so this module never has to fail on a
/// missing row — that would be a caller bug, not a Home-screen concern.
#[derive(Clone)]
struct SyncCtx {
    pool: SqlitePool,
    paths: AppPaths,
    server: Server,
    account: Account,
    session: abs_core::auth::Session,
}

#[derive(Clone)]
struct HomeData {
    libraries: Vec<Library>,
    recent_items: Vec<Item>,
    continue_items: Vec<(Item, Progress)>,
    /// Ids of items downloaded fully or partially — one query per load, checked per card rather
    /// than a query per card. Feeds `item_card::build`'s read-only download badge, and (when
    /// `offline_mode` is on) filters the shelves to just these items.
    downloaded: std::collections::HashSet<String>,
}

/// Builds the screen. `server`/`account` are already-resolved rows (the caller, `main_window`,
/// looks them up once) rather than bare ids, so this module never has to fail on a missing
/// server/account — that would be a caller bug, not a Home-screen concern. `on_open` is how
/// tapping a cover card opens the Item Detail screen for it — Home never touches
/// `abs-player`/`abs-core::streaming` itself, matching the `on_success`-callback pattern
/// `welcome.rs` already uses. `on_relogin` is
/// how the authorization-failure states hand control back to the shell: it swaps the window's
/// content for a pre-filled login screen (the session that just died is the shell's knowledge,
/// not Home's). `on_open_shelf` is the shelf headings' tap-through: it navigates to the Library
/// pre-sorted (and pre-filtered, for Continue Listening) — the shell owns that navigation, Home
/// only reports which shelf was tapped.
#[allow(clippy::too_many_arguments)]
pub fn build(
    pool: SqlitePool,
    paths: AppPaths,
    server: Server,
    account: Account,
    session: abs_core::auth::Session,
    offline_mode: crate::offline_mode::OfflineModeState,
    on_open: impl Fn(PlayRequest) + Clone + 'static,
    on_relogin: impl Fn() + Clone + 'static,
    on_open_shelf: impl Fn(Shelf) + Clone + 'static,
) -> HomeScreen {
    let header = adw::HeaderBar::new();
    let home_title = adw::WindowTitle::new(&tr("Home"), "");
    crate::a11y::landing(&home_title, &tr("Home"));
    header.set_title_widget(Some(&home_title));

    let avatar_letter = account
        .username
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string()); // i18n: ignore
    // There's no account-switcher/sign-out screen built yet (per `docs/design/ui-spec.md`, that
    // lives in Settings' Servers group, still a stub tab) — until it exists, this button can't
    // actually do anything, so a tooltip is the honest fix: it should read as "this is who I'm
    // signed in as", not as a dead button whose purpose is a mystery.
    let avatar = gtk4::Button::builder()
        .css_classes(["circular", "suggested-action"])
        .valign(gtk4::Align::Center)
        // TRANSLATORS: {username} is the name of the account signed in on this device.
        .tooltip_text(tr_args("Signed in as {username}", &[("username", account.username.as_str())]))
        .child(&gtk4::Label::new(Some(&avatar_letter)))
        .build();
    // Not a Tab stop: it does nothing yet. Its name still says who is signed in.
    avatar.set_focusable(false);
    avatar.set_accessible_role(gtk4::AccessibleRole::Status);
    crate::a11y::name(&avatar, &tr_args("Signed in as {username}", &[("username", account.username.as_str())]));
    header.pack_end(&avatar);

    // "Sync now" (ui-spec Home): the header-bar ⋯ overflow — the one page-level action this
    // screen needs — forcing an immediate resync instead of waiting on the next tab-entry or
    // retry cycle. The pull-to-refresh gesture on the main scroller (wired below) runs the
    // exact same manual path. Plain `GtkMenuButton` + `GtkPopover` + flat button, matching the
    // no-GMenu convention everywhere else in this crate.
    let sync_now_button = gtk4::Button::builder().label(tr("Sync now")).css_classes(["flat"]).build();
    let sync_menu_popover = gtk4::Popover::builder().child(&sync_now_button).build();
    let sync_menu_button = gtk4::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text(tr("More"))
        .popover(&sync_menu_popover)
        .build();
    crate::a11y::name(&sync_menu_button, &tr("More"));
    header.pack_end(&sync_menu_button);

    // Offline-mode toggle (ui-spec: "leading side, opposite the avatar"). Shared persisted state
    // with Library's own toggle — see `HomeWidgets::offline_mode`'s field doc.
    let offline_toggle = gtk4::ToggleButton::builder().icon_name("airplane-mode-symbolic").tooltip_text(tr("Offline mode")).build();
    crate::a11y::switch_like(&offline_toggle);
    crate::a11y::name(&offline_toggle, &tr("Offline mode"));
    header.pack_start(&offline_toggle);

    let offline_banner_label = gtk4::Label::builder().label(tr("Showing downloaded items only")).xalign(0.0).hexpand(true).css_classes(["caption", "dim-label"]).build();
    let offline_banner = gtk4::Revealer::builder().transition_type(gtk4::RevealerTransitionType::SlideDown).child(&offline_banner_label).reveal_child(false).build();

    let banner = crate::widgets::banner::ErrorBanner::new();

    let continue_row = shelf_row();
    let continue_section = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .visible(false)
        .build();
    let continue_heading = section_heading_button(
        &tr("Continue Listening"),
        &tr("Show in-progress books in the Library"),
        {
            let on_open_shelf = on_open_shelf.clone();
            move || on_open_shelf(Shelf::ContinueListening)
        },
    );
    continue_section.append(&continue_heading);
    continue_section.append(&shelf_scroller(&continue_row));

    let recent_row = shelf_row();
    let recent_section = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    let recent_heading = section_heading_button(&tr("Recently Added"), &tr("Show the Library by date added"), {
        let on_open_shelf = on_open_shelf.clone();
        move || on_open_shelf(Shelf::RecentlyAdded)
    });
    recent_section.append(&recent_heading);
    recent_section.append(&shelf_scroller(&recent_row));

    let libraries_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(16)
        .margin_end(16)
        .margin_top(2)
        .margin_bottom(18)
        .build();
    let libraries_section = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    libraries_section.append(&section_heading(&tr("Your Libraries")));
    libraries_section.append(&libraries_list);

    let empty_state = EmptyState::build();

    let scroll_content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    scroll_content.append(&continue_section);
    scroll_content.append(&recent_section);
    scroll_content.append(&libraries_section);

    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .visible(false)
        .child(&scroll_content)
        .build();

    // The banner lives directly under the header bar, outside the scroller, so a sync failure is
    // visible in every state — when the shelves are empty the scroller is hidden, and a banner
    // trapped inside it was exactly how the first-sync failure used to disappear without a trace.
    let (pull_indicator_widget, pull_indicator) = crate::widgets::pull_to_refresh::PullIndicator::build();
    let body = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    body.append(&offline_banner);
    body.append(banner.widget());
    body.append(&pull_indicator_widget);
    body.append(&scroller);
    body.append(&empty_state.root);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&body);

    // Toasts float over the whole screen, header bar included — same shape as the player
    // screen's own overlay. Manual sync triggers report through this one.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&root));
    crate::a11y::focus_landing(&home_title, &toast_overlay);

    let widgets = HomeWidgets {
        empty_state: empty_state.clone(),
        scroller: scroller.clone(),
        continue_section: continue_section.clone(),
        continue_row: continue_row.clone(),
        recent_row: recent_row.clone(),
        libraries_list: libraries_list.clone(),
        banner: banner.clone(),
        on_open: std::rc::Rc::new(on_open),
        offline_mode,
        last_data: std::rc::Rc::new(std::cell::RefCell::new(None)),
    };

    // The screen starts in the syncing state (EmptyState's construction default): a returning
    // session's cached render lands within the first few frames of the cycle below and takes
    // over, while a fresh login keeps the spinner up for as long as the sync actually runs.
    let ctx = SyncCtx {
        pool: pool.clone(),
        paths: paths.clone(),
        server: server.clone(),
        account: account.clone(),
        session: session.clone(),
    };
    spawn_sync_cycle(ctx.clone(), widgets.clone(), None);

    // Offline mode turned off: catch up with the server. Both screens ask; the startup-sync claim
    // lets only one of them hit the network, and the other follows (see `sync_coordinator`). A
    // server added while offline mode was on used to stay empty until "Try again".
    widgets.offline_mode.add_listener({
        let (ctx, widgets) = (ctx.clone(), widgets.clone());
        let was_offline = std::cell::Cell::new(ctx.session.is_offline());
        move |offline| {
            let came_online = was_offline.replace(offline) && !offline;
            if came_online && widgets.scroller.root().is_some() {
                spawn_sync_cycle(ctx.clone(), widgets.clone(), None);
            }
        }
    });

    // Library's own syncs ("Sync now" there, the catch-up after offline mode) land here too.
    // Dropped once this screen is no longer in a window (the shell was rebuilt).
    crate::sync_coordinator::on_synced(&server.id, &account.id, crate::sync_coordinator::Screen::Home, {
        let (pool, server_id, account_id, widgets) = (pool.clone(), server.id.clone(), account.id.clone(), widgets.clone());
        move |outcome| {
            if widgets.scroller.root().is_none() {
                return false;
            }
            glib::spawn_future_local(follow_library_sync(pool.clone(), server_id.clone(), account_id.clone(), widgets.clone(), Some(outcome)));
            true
        }
    });

    // The two manual triggers — the ⋯ menu's "Sync now" and a pull past the scroller's top —
    // share one `ManualSync` (indicator + toast + in-flight guard) between them; see
    // `spawn_sync_cycle`.
    let manual_sync = crate::widgets::ManualSync::new(&toast_overlay, &pull_indicator);

    // Try again re-runs the whole cycle: back to the spinner first, then the same
    // sync → render → resolve pipeline the screen opened with. Unguarded and toast-less like
    // the automatic cycle — its feedback is the empty state it just reset.
    {
        let ctx = ctx.clone();
        let widgets = widgets.clone();
        empty_state.retry.connect_clicked(move |_| {
            widgets.empty_state.show_syncing();
            widgets.banner.set_revealed(false);
            spawn_sync_cycle(ctx.clone(), widgets.clone(), None);
        });
    }

    {
        let ctx = ctx.clone();
        let widgets = widgets.clone();
        let manual_sync = manual_sync.clone();
        let popover = sync_menu_popover.clone();
        sync_now_button.connect_clicked(move |_| {
            popover.popdown();
            spawn_sync_cycle(ctx.clone(), widgets.clone(), Some(manual_sync.clone()));
        });
    }
    {
        let ctx = ctx.clone();
        let widgets = widgets.clone();
        let manual_sync = manual_sync.clone();
        crate::widgets::pull_to_refresh::attach(&scroller, move || {
            spawn_sync_cycle(ctx.clone(), widgets.clone(), Some(manual_sync.clone()));
        });
    }

    // "Log in again" — from the failure state or the banner — hands control to the shell, which
    // swaps the window's content for a login screen pre-filled with this session's URL/username.
    {
        let on_relogin = std::rc::Rc::new(on_relogin);
        empty_state.login_again.connect_clicked({
            let on_relogin = on_relogin.clone();
            move |_| on_relogin()
        });
        // The banner's one action is "Retry" or "Log in again", whichever its failure offers.
        let ctx = ctx.clone();
        let widgets = widgets.clone();
        let manual_sync = manual_sync.clone();
        banner.action_button().connect_clicked(move |_| {
            if widgets.banner.offers_login() {
                on_relogin();
            } else {
                spawn_sync_cycle(ctx.clone(), widgets.clone(), Some(manual_sync.clone()));
            }
        });
    }

    // Shrunk to just forwarding into the shared state — every visible effect (banner, re-render,
    // background refetch, persistence) now lives in the `add_listener` callback below, since that
    // same logic must run whether *this* screen's own toggle fired or Library's did.
    let offline_toggle_handler = offline_toggle.connect_toggled({
        let offline_mode = widgets.offline_mode.clone();
        move |toggle| {
            let _slow = crate::perf::SlowJob::new("offline mode toggle");
            offline_mode.set(toggle.is_active());
        }
    });

    widgets.offline_mode.add_listener({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let offline_toggle = offline_toggle.clone();
        let offline_banner = offline_banner.clone();
        let server_id = server.id.clone();
        let toast_overlay = toast_overlay.clone();
        move |active| {
            // Sync the toggle widget's visual state without re-triggering `connect_toggled`. When
            // this screen's own toggle caused the change, `is_active()` already equals `active`
            // (GTK flips it before `connect_toggled` runs), so this is a no-op here and only
            // actually touches the widget when the *other* screen changed it.
            if offline_toggle.is_active() != active {
                offline_toggle.block_signal(&offline_toggle_handler);
                offline_toggle.set_active(active);
                offline_toggle.unblock_signal(&offline_toggle_handler);
            }
            offline_banner.set_reveal_child(active);
            // Rendered on the next idle, before any DB work — the refetch's pool acquire must not
            // gate the visible filter change (a contended pool has stalled it for tens of seconds
            // on-device). Not inside this handler: GTK only redraws the toggle after the handler
            // returns, and rebuilding the shelves first made the toggle seem not to react. The
            // in-memory snapshot is immediate-and-slightly-stale; the spawned refetch below
            // re-renders with fresh data when it lands.
            glib::idle_add_local_once({
                let widgets = widgets.clone();
                move || {
                    let cached = widgets.last_data.borrow().clone();
                    if let Some(data) = cached {
                        apply(&data, &widgets);
                    }
                }
            });
            glib::spawn_future_local({
                let pool = pool.clone();
                let widgets = widgets.clone();
                let server_id = server_id.clone();
                let toast_overlay = toast_overlay.clone();
                async move {
                    // Refetched here rather than trusting whatever the last sync's snapshot saw —
                    // a download can complete in the background well after the last sync.
                    match abs_core::download_tracks::downloaded_item_ids(&pool, &server_id).await {
                        Ok(downloaded) => {
                            if let Some(data) = widgets.last_data.borrow_mut().as_mut() {
                                data.downloaded = downloaded.into_iter().collect();
                            }
                        }
                        Err(err) => crate::error_reporting::report_background_error(&toast_overlay, &tr("Refreshing downloaded items"), err),
                    }
                    // Borrowed and cloned in its own statement, not inline in the `if let`'s
                    // scrutinee — a `Ref` there lives for the whole `if let` body (temporary
                    // lifetime extension), so calling `apply()` (which itself borrows
                    // `last_data` mutably) while still inside that scrutinee's `if let` panics
                    // with "already borrowed" — caught live via the visual/manual verification
                    // pass, not by any test.
                    let cached = widgets.last_data.borrow().clone();
                    if let Some(data) = cached {
                        apply(&data, &widgets);
                    }
                }
            });
        }
    });

    HomeScreen {
        root: toast_overlay.clone().upcast(),
        widgets: widgets.clone(),
        pool: pool.clone(),
        server_id: server.id.clone(),
        account_id: account.id.clone(),
        #[cfg(test)]
        hooks: TestHooks {
            empty_state,
            libraries_list,
            continue_section,
            continue_heading,
            continue_row,
            recent_row,
            recent_heading,
            banner,
            offline_toggle,
            offline_banner,
            sync_now_button,
            toast_overlay,
            scroller,
            pull_spinner: pull_indicator.spinner().clone(),
        },
    }
}

/// Runs one full sync cycle on the main loop: render whatever's cached, sync against the server
/// on worker threads, render again, then resolve the empty-state/banner from the outcome. Called
/// once when the screen is built, again on every "Try again" press, and on every *manual* sync
/// trigger (the ⋯ menu's "Sync now", pull-to-refresh — see `manual`), so it must leave all
/// widget state consistent no matter how many times it runs.
///
/// A manual trigger (`manual`) additionally reports the outcome as a transient toast and shares
/// an in-flight guard with the screen's other manual trigger — see `crate::widgets::ManualSync`.
/// The automatic cycle and "Try again" pass none and stay unguarded.
///
/// The network/DB pipeline runs inside `tokio::spawn` (worker threads), not directly in this
/// `spawn_future_local` future: the latter is polled on the GTK main thread, so HTTP body chunk
/// handling, statement building and cache-file writes done directly here steal frames from the
/// main loop — visibly, when dozens of per-item cover fetches all poll at once (observed as a
/// multi-second UI hang while scrolling the Continue Listening shelf). The main context only
/// parks on the `JoinHandle`s, which costs nothing to await, and widget updates happen back on
/// this thread between stages.
///
/// Rendering is gated on there being at least one library: shelves with nothing in them look
/// broken, and the empty state below is the honest rendering of "nothing to show yet".
fn spawn_sync_cycle(ctx: SyncCtx, widgets: HomeWidgets, manual: Option<crate::widgets::ManualSync>) {
    // A manual trigger while its sibling is still running is a no-op: an overshot can fire
    // repeatedly during one rubber-band, and the ⋯ menu is one tap away from the gesture.
    // Overlapping syncs wouldn't corrupt anything (every write is an idempotent upsert), but
    // they'd race each other's covers-fetches and double-toast — not worth it.
    if let Some(manual) = &manual {
        if !manual.claim() {
            return;
        }
    }

    // Offline mode: nothing may reach the server — show what's stored locally and stop there.
    // No sync claim is taken, so Library (which does the same) never waits on one.
    if ctx.session.is_offline() {
        tracing::info!("offline mode: Home shows what's stored on the device; not syncing");
        let (pool, server_id, account_id) = (ctx.pool.clone(), ctx.server.id.clone(), ctx.account.id.clone());
        glib::spawn_future_local(async move {
            match load(&pool, &server_id, &account_id).await {
                Ok(data) if !data.libraries.is_empty() => apply(&data, &widgets),
                _ => widgets.empty_state.show_offline_empty(),
            }
        });
        if let Some(manual) = manual {
            manual.finish_offline();
        }
        return;
    }

    // An *automatic* cycle (no `manual`) is what `build()` fires once, immediately — and Library
    // does the exact same thing for the same account, since both screens are built eagerly. Only
    // one of them should actually hit the network; see `sync_coordinator`'s module doc. A manual
    // trigger is user-initiated and always runs for real regardless — nothing else races it.
    let is_automatic = manual.is_none();
    if is_automatic {
        let server_id = ctx.server.id.clone();
        let account_id = ctx.account.id.clone();
        if !crate::sync_coordinator::claim_startup_sync(&server_id, &account_id) {
            let pool = ctx.pool.clone();
            glib::spawn_future_local(render_from_cache(pool.clone(), server_id.clone(), account_id.clone(), widgets.clone()));
            // Separate bindings for the borrow `on_completed` takes vs. what the closure below
            // moves — borrowing `server_id`/`account_id` themselves here would conflict with the
            // `move` closure's own capture of them, evaluated as part of the same call.
            let (claim_server_id, claim_account_id) = (server_id.clone(), account_id.clone());
            crate::sync_coordinator::on_completed(&claim_server_id, &claim_account_id, move |outcome| {
                glib::spawn_future_local(follow_library_sync(pool, server_id, account_id, widgets, outcome));
            });
            return;
        }
    }

    glib::spawn_future_local(async move {
        let SyncCtx { pool, paths, server, account, session } = ctx;
        let server_id = server.id.clone();
        let account_id = account.id.clone();
        // One log line per load, step by step (`home load finished …`) — see `perf::Steps`.
        let mut steps = crate::perf::Steps::new("home load", if manual.is_some() { "manual" } else { "automatic" });

        steps.step("cached read");
        let cached = load(&pool, &server_id, &account_id).await;
        steps.step("cached render");
        if let Ok(data) = &cached {
            if !data.libraries.is_empty() {
                apply(data, &widgets);
            }
        }
        steps.close();

        let spawned_sync = tokio::spawn({
            let pool = pool.clone();
            let session = session.clone();
            let sync_paths = paths.clone();
            let server_id = server_id.clone();
            let account_id = account_id.clone();
            // Asked at call time, not captured at build time — a token captured here would
            // be the one from whenever this screen was constructed, and on servers v2.26.0+
            // it dies within hours while the app stays open. The connection (settings +
            // resolved base URL) is asked the same way, so a settings change is honored by
            // the next sync cycle.
            async move {
                let access_token = session.access_token().await;
                let connection = match session.connection_target().await {
                    Ok(connection) => connection,
                    Err(err) => {
                        tracing::warn!(%err, "couldn't load the server's connection settings; sync skipped");
                        let data = Some(load(&pool, &server_id, &account_id).await);
                        return (Err(err), data, [std::time::Duration::ZERO; 3]);
                    }
                };
                let started = std::time::Instant::now();
                let sync_result = abs_core::sync::sync_all(&pool, &connection, &sync_paths, &server_id, &access_token).await;
                let sync_took = started.elapsed();
                let started = std::time::Instant::now();

                // Reconciling "Continue Listening" against the server's progress runs after
                // sync_all, not concurrently with it: an item's progress can only be attached
                // once the item itself has been synced locally (a fresh login has no local
                // items at all yet). It's still best-effort and bounded by its own short
                // timeout — a failure here (offline, slow connection) is logged and never
                // surfaced as this screen's sync banner, which is about library/item sync,
                // not this.
                if let Err(err) = abs_core::progress_sync::reconcile_all_progress(&pool, &connection, &access_token, &account_id, &server_id, crate::sync_coordinator::loaded_item(&server_id, &account_id).as_deref()).await
                {
                    tracing::warn!(%err, "couldn't reconcile Continue Listening progress with the server; showing local progress");
                }

                let reconcile_took = started.elapsed();

                let started = std::time::Instant::now();
                let data_after_sync = Some(load(&pool, &server_id, &account_id).await);
                (sync_result, data_after_sync, [sync_took, reconcile_took, started.elapsed()])
            }
        });
        let (sync_result, data_after_sync, [sync_took, reconcile_took, reread_took]) = spawned_sync
            .await
            .expect("the Home sync task must not panic");
        crate::sync_coordinator::network_used(&server_id, &account_id);
        steps.record("sync", sync_took);
        steps.record("reconcile", reconcile_took);
        steps.record("reread", reread_took);
        // Offline mode switched on mid-sync cut it short; that's not a failure to show.
        let sync_result = sync_result.or_else(|err| if matches!(err, CoreError::Offline) { Ok(()) } else { Err(err) });
        let local_read_error = data_after_sync.as_ref().and_then(|data| data.as_ref().err()).map(|err| {
            tracing::warn!(%err, "couldn't read the synced library back from local storage");
            err.to_string()
        });
        let data_after_sync = data_after_sync.and_then(Result::ok);
        steps.step("rerender");
        if let Some(data) = &data_after_sync {
            if !data.libraries.is_empty() {
                apply(data, &widgets);
            }
        }
        steps.close();

        // Cover art is cosmetic and best-effort (same posture as `abs_core::covers` already
        // uses for the player screen) — fetched concurrently for every item just rendered,
        // after the rest of the screen is already showing, so a slow/offline server delays
        // only the artwork, never the initial render. The whole batch shares one HTTP client
        // (one pooled connection), and per item it no-ops once a cover is already cached on
        // disk, so this is cheap on every subsequent visit.
        // A manual sync also re-asks for covers the server recently said it doesn't have.
        let recheck_missing_covers = manual.is_some();
        let spawned_covers = data_after_sync.as_ref().filter(|data| !data.libraries.is_empty()).map(|data| {
            let item_ids: Vec<String> = data
                .recent_items
                .iter()
                .map(|item| item.id.clone())
                .chain(data.continue_items.iter().map(|(item, _)| item.id.clone()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            tokio::spawn({
                let pool = pool.clone();
                let paths = paths.clone();
                let session = session.clone();
                let server_id = server_id.clone();
                let account_id = account_id.clone();
                async move {
                    let access_token = session.access_token().await;
                    // Best-effort like the fetches themselves: a settings failure here just
                    // means no covers — logged, never surfaced.
                    if let Some(connection) = session.connection_target().await.ok().as_ref() {
                        abs_core::covers::fetch_and_cache_covers(&paths, &pool, connection, &access_token, &server_id, item_ids, recheck_missing_covers).await;
                    }

                    load(&pool, &server_id, &account_id).await.ok()
                }
            })
        });
        if let Some(spawned_covers) = spawned_covers {
            steps.step("covers");
            let data_after_covers = spawned_covers.await.expect("the Home cover-fetch task must not panic");
            steps.step("final render");
            if let Some(data) = data_after_covers {
                if !data.libraries.is_empty() {
                    apply(&data, &widgets);
                }
            }
        }
        steps.finish(if sync_result.is_ok() { "ok" } else { "failed" }, None);

        // Resolve the screen's final state for this cycle from the sync outcome plus whatever
        // actually landed in local storage. The two visible outcomes are mutually exclusive:
        // with data, the banner (if anything, the partial-failure case); without, the empty
        // state carries the whole story — which is why it, not the banner, owns the no-data
        // failure mode. Auth failures get their own copy and a "Log in again" action in both
        // places: they're the one failure retrying can't fix.
        let manual_ok = sync_result.is_ok();
        let has_data = data_after_sync.as_ref().is_some_and(|data| !data.libraries.is_empty());
        show_sync_result(&widgets, has_data, sync_result.as_ref().err(), local_read_error.as_deref());

        // The manual trigger's own feedback — a transient outcome toast — lands only after the
        // resolve above, so the detailed surface (banner/empty state) is already showing
        // whatever the toast is about. "Sync failed" carries no details itself: those live in
        // the banner/empty state, which this same cycle just set.
        if let Some(manual) = manual {
            manual.finish(manual_ok);
        }

        // Only the cycle that actually claimed the automatic sync (see above) reports back —
        // Library, if it lost the claim, is waiting on exactly this to re-render from what just
        // landed.
        if is_automatic {
            crate::sync_coordinator::mark_completed(&server_id, &account_id, crate::sync_coordinator::SyncOutcome::from_result(&sync_result));
        } else {
            crate::sync_coordinator::notify_synced(&server_id, &account_id, crate::sync_coordinator::Screen::Home, crate::sync_coordinator::SyncOutcome::from_result(&sync_result));
        }
    });
}

/// Shows how a sync ended: with saved data to keep showing, on the banner (if anything went
/// wrong); without, on the full-page state, which then carries the whole story. Auth failures
/// get their own copy and a "Log in again" action in both places: they're the one failure
/// retrying can't fix. Called by whichever of Home and Library ran the startup sync and by the
/// one that waited for it (`sync_coordinator::SyncOutcome`), so both tell the same story.
fn show_sync_result(widgets: &HomeWidgets, has_data: bool, error: Option<&CoreError>, local_read_error: Option<&str>) {
    if has_data {
        widgets.empty_state.hide();
        match error {
            None => widgets.banner.set_revealed(false),
            Some(err) => widgets.banner.show_sync_failure(err),
        }
    } else {
        widgets.banner.set_revealed(false);
        match error {
            None => match local_read_error {
                Some(error) => widgets.empty_state.show_local_read_error(error),
                None => widgets.empty_state.show_empty(),
            },
            Some(err @ CoreError::Auth) => widgets.empty_state.show_auth_error(&err.to_string()),
            Some(err) => widgets.empty_state.show_error(err),
        }
    }
}

/// Reads whatever's currently cached locally and renders it — never talks to the network. Used
/// both as the ordinary "show cached data immediately, before syncing" step of a real cycle, and
/// as the *entire* response for a screen that lost the automatic-sync claim (see
/// `sync_coordinator`): called once right away and once more when the winning screen's own cycle
/// completes.
async fn render_from_cache(pool: SqlitePool, server_id: String, account_id: String, widgets: HomeWidgets) {
    if let Ok(data) = load(&pool, &server_id, &account_id).await {
        if !data.libraries.is_empty() {
            apply(&data, &widgets);
        }
    }
}

/// Re-reads local storage after Library ran a sync (the startup one Home didn't claim, or a
/// manual one) and shows how it went — so this screen never just redraws silently (or keeps its
/// first-sync spinner) after a failure.
async fn follow_library_sync(pool: SqlitePool, server_id: String, account_id: String, widgets: HomeWidgets, outcome: Option<crate::sync_coordinator::SyncOutcome>) {
    let mut steps = crate::perf::Steps::new("home load", "after Library's sync");
    steps.step("read");
    let loaded = load(&pool, &server_id, &account_id).await;
    let has_data = loaded.as_ref().is_ok_and(|data| !data.libraries.is_empty());
    steps.step("render");
    if let (Ok(data), true) = (&loaded, has_data) {
        apply(data, &widgets);
    }
    steps.finish(if loaded.is_ok() { "ok" } else { "failed" }, None);
    if let Some(outcome) = outcome {
        let local_read_error = loaded.as_ref().err().map(|err| err.to_string());
        show_sync_result(&widgets, has_data, outcome.error(), local_read_error.as_deref());
    }
}

/// Reads whatever's currently cached locally — never talks to the network. Called once before
/// syncing (to show cached data immediately) and once after (to pick up whatever the sync just
/// wrote).
async fn load(pool: &SqlitePool, server_id: &str, account_id: &str) -> CoreResult<HomeData> {
    let libraries = abs_storage::repo::libraries::list_for_server(pool, server_id).await?;

    let mut recent_items = Vec::new();
    for library in &libraries {
        recent_items.extend(abs_storage::repo::items::list_for_library(pool, server_id, &library.id).await?);
    }
    recent_items.sort_by_key(|a| std::cmp::Reverse(a.added_at));
    recent_items.truncate(10);

    let mut continue_items = Vec::new();
    for progress in abs_storage::repo::progress::list_recent_for_account(pool, account_id, 10).await? {
        // Finished books aren't "continue" listening — a book completed years ago would
        // otherwise squat on the shelf forever. It stays visible in the library, with progress.
        if progress.is_finished {
            continue;
        }
        // A progress row can outlive the item it points at (e.g. removed from the server between
        // syncs) — skip it rather than failing the whole Home screen over one stale row.
        if let Ok(item) = abs_storage::repo::items::get(pool, server_id, &progress.item_id).await {
            continue_items.push((item, progress));
        }
    }

    let downloaded = abs_core::download_tracks::downloaded_item_ids(pool, server_id).await?.into_iter().collect();

    Ok(HomeData { libraries, recent_items, continue_items, downloaded })
}

/// Rebuilds every dynamic widget from `data`. Safe to call repeatedly — clears each container's
/// children first, so this is a full re-render rather than an incremental diff (fine at this
/// scale: a handful of shelf cards and library rows, not a large list needing virtualization).
fn apply(data: &HomeData, widgets: &HomeWidgets) {
    let _slow = crate::perf::SlowJob::new("home render");
    *widgets.last_data.borrow_mut() = Some(data.clone());

    // The empty state's *mode* is owned by the sync-cycle state machine, but its hiding happens
    // here, the moment real content renders — not later in the cycle (after the best-effort
    // cover fetches), so the spinner state never briefly coexists with the shelves.
    if !data.libraries.is_empty() {
        widgets.empty_state.hide();
    }
    // The empty state's visibility overall is owned by the sync-cycle state machine, not here —
    // apply only ever has something (or nothing) to put in the scroller.
    widgets.scroller.set_visible(!data.libraries.is_empty());

    let offline_mode = widgets.offline_mode.get();

    clear_box(&widgets.continue_row);
    let continue_items: Vec<_> = data.continue_items.iter().filter(|(item, _)| !offline_mode || data.downloaded.contains(&item.id)).collect();
    for (item, progress) in &continue_items {
        let fraction = if item.duration_seconds > 0.0 { (progress.current_time_seconds / item.duration_seconds).clamp(0.0, 1.0) } else { 0.0 };
        // The card's progress row shows the percentage, so the subtitle is just the author.
        let unknown_author = tr("Unknown author");
        let subtitle = item.author.as_deref().unwrap_or(&unknown_author);
        widgets.continue_row.append(&item_card::build(132, item, subtitle, &widgets.on_open, false, data.downloaded.contains(&item.id), Some(fraction)));
    }
    widgets.continue_section.set_visible(!continue_items.is_empty());

    clear_box(&widgets.recent_row);
    // "Libraries" itself isn't filtered — a library entry isn't a downloadable item, only the
    // items inside it are (ui-spec's "Libraries list" filtering is interpreted here as "still
    // browsable", not "hidden entirely", since there's no per-library download concept).
    for item in data.recent_items.iter().filter(|item| !offline_mode || data.downloaded.contains(&item.id)) {
        let subtitle = item_subtitle(item);
        widgets.recent_row.append(&item_card::build(132, item, &subtitle, &widgets.on_open, false, data.downloaded.contains(&item.id), None));
    }

    clear_listbox(&widgets.libraries_list);
    // A library the server no longer lists is kept only for downloaded books, which show on the
    // shelves and in Library; it isn't one of "your libraries" any more.
    for library in data.libraries.iter().filter(|library| !library.removed_on_server) {
        widgets.libraries_list.append(&library_row(library));
    }
}

fn item_subtitle(item: &Item) -> String {
    let unknown_author = tr("Unknown author");
    let author = item.author.as_deref().unwrap_or(&unknown_author);
    let duration = crate::screens::item_detail::format_duration(item.duration_seconds);
    // TRANSLATORS: A book's subtitle on a card or in a list. {author} is the author's name; {duration} its length, e.g. "11.5h" or "45m".
    tr_args("{author} · {duration}", &[("author", author), ("duration", &duration)])
}

fn section_heading(text: &str) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_start(16)
        .margin_end(16)
        .margin_top(18)
        .margin_bottom(8)
        .build();
    crate::a11y::heading(&label, 2);
    label
}

/// A shelf heading that's also tappable (ui-spec Home tap-through): visually the same heading
/// (`flat` chrome over an identically-styled label, full row width so the touch target isn't
/// just the text), with a tooltip explaining where it goes.
fn section_heading_button(text: &str, tooltip: &str, on_open: impl Fn() + 'static) -> gtk4::Button {
    let label = gtk4::Label::builder().label(text).xalign(0.0).css_classes(["heading"]).build();
    let button = gtk4::Button::builder()
        .child(&label)
        .tooltip_text(tooltip)
        .css_classes(["flat"])
        .margin_start(16)
        .margin_end(16)
        .margin_top(18)
        .margin_bottom(8)
        .build();
    // The heading is the button's name (a screen reader otherwise reads only the tooltip).
    crate::a11y::name(&button, text);
    crate::a11y::describe(&button, tooltip);
    button.connect_clicked(move |_| on_open());
    button
}

fn shelf_row() -> gtk4::Box {
    let row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(12)
        .margin_start(16)
        .margin_end(16)
        .margin_bottom(4)
        .build();
    // One Tab stop for the whole shelf; Left/Right move between its covers.
    crate::a11y::focus_group(&row);
    row
}

fn shelf_scroller(row: &gtk4::Box) -> gtk4::ScrolledWindow {
    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        // Without this, a `GtkScrolledWindow` with vertical scrolling disabled still only
        // requests its own default minimum height, not its child's actual (wrapped-label-and-all)
        // natural height — the row's second line of text (author/duration) was getting clipped
        // at the bottom of the shelf rather than being fully shown. This tells it to size to fit.
        .propagate_natural_height(true)
        .child(row)
        .build();
    // The mouse wheel over a shelf scrolls Home, not nothing.
    crate::widgets::sideways_scroll::pass_vertical_scroll_to_page(&scroller);
    scroller
}

fn library_row(library: &Library) -> adw::ActionRow {
    let icon_name = if library.media_type == "podcast" { "audio-input-microphone-symbolic" } else { crate::icons::LIBRARY };
    // The server's `mediaType` value; the two known kinds are translated, anything else is shown as sent.
    let kind = match library.media_type.as_str() {
        // TRANSLATORS: kind of an Audiobookshelf library, shown under its name on Home. Lowercase in English.
        "book" => tr_ctx("library kind", "book"),
        // TRANSLATORS: shown under a podcast library's name on Home; the app can't play podcasts, so their shows aren't listed anywhere.
        "podcast" => tr("Podcasts aren't supported yet"),
        other => other.to_string(),
    };
    let row = adw::ActionRow::builder().title(library.name.as_str()).subtitle(kind.as_str()).build();
    row.add_prefix(&gtk4::Image::from_icon_name(icon_name));
    row
}

fn clear_box(b: &gtk4::Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

fn clear_listbox(lb: &gtk4::ListBox) {
    while let Some(child) = lb.row_at_index(0) {
        lb.remove(&child);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::{pool, pump_until};
    use abs_storage::repo::libraries::UpsertLibrary;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(crate) async fn account_and_server(pool: &SqlitePool, server_url: &str) -> (Server, Account) {
        let server_id = abs_storage::repo::servers::add(pool, server_url).await.unwrap();
        let account_id = abs_storage::repo::accounts::add(pool, &server_id, "jane", "token123", None).await.unwrap();
        (
            abs_storage::repo::servers::get(pool, &server_id).await.unwrap(),
            abs_storage::repo::accounts::get(pool, &account_id).await.unwrap(),
        )
    }

    pub(crate) fn item_json(id: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "addedAt": 1_700_000_000_000i64,
            "media": {
                "duration": 3600.0,
                "metadata": { "title": title, "authorName": "Andy Weir" }
            }
        })
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point.
    pub(crate) fn run_renders_synced_library_and_recently_added_item(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        // `empty_state` starts in the syncing state (only shown while nothing is cached), so
        // waiting on its *hiding* can't distinguish "sync hasn't run yet" from "sync ran and
        // found nothing" — wait on the actual data landing instead.
        pump_until(|| hooks.libraries_list.row_at_index(0).is_some(), Duration::from_secs(10));
        // Both shelves pass the mouse wheel on to the page (see `widgets::sideways_scroll`).
        for row in [&hooks.continue_row, &hooks.recent_row] {
            let shelf = row.ancestor(gtk4::ScrolledWindow::static_type()).and_downcast::<gtk4::ScrolledWindow>().unwrap();
            assert!(crate::widgets::sideways_scroll::tests::passes_vertical_scroll_on(&shelf));
        }

        assert!(!hooks.empty_state.root.is_visible(), "should have left the empty state once a library synced");
        assert!(hooks.libraries_list.row_at_index(0).is_some(), "the synced library should have a row");
        assert!(hooks.recent_row.first_child().is_some(), "the synced item should show under Recently Added");
        assert!(!hooks.continue_section.is_visible(), "no progress exists yet, so Continue Listening stays hidden");
    }

    pub(crate) fn count_children(b: &gtk4::Box) -> usize {
        let mut count = 0;
        let mut child = b.first_child();
        while let Some(widget) = child {
            count += 1;
            child = widget.next_sibling();
        }
        count
    }

    /// A finished book must not claim a Continue Listening slot, even when its progress row is
    /// the most recently updated one — the shelf is for picking up where you left off.
    pub(crate) fn run_finished_books_dont_show_under_continue_listening(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary"), item_json("item-2", "Dune")]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        // The progress table's foreign key needs the items to exist locally before the first
        // sync populates them, so seed the same ids the mock server will return.
        let library_id = "e4bb1afb-4a4f-4dd6-8be0-e615d233185b";
        runtime.block_on(abs_storage::repo::libraries::upsert(&pool, UpsertLibrary { id: library_id, server_id: &server.id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 })).unwrap();
        for (id, title) in [("item-1", "Project Hail Mary"), ("item-2", "Dune")] {
            runtime.block_on(abs_storage::repo::items::upsert(&pool, abs_storage::repo::items::UpsertItem { id, server_id: &server.id, library_id, title, author: None, narrator: None, description: None, duration_seconds: 3600.0, added_at: chrono::Utc::now(), series_name: None, genres: &[] })).unwrap();
        }

        // The finished book's row is the *newer* one, so without the finished filter it would be
        // the first card on the shelf.
        let now = chrono::Utc::now();
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 1800.0, false)).unwrap();
        runtime.block_on(abs_storage::repo::progress::set_at(&pool, &account.id, &server.id, "item-2", 3600.0, true, now)).unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        pump_until(|| count_children(&hooks.continue_row) == 1, Duration::from_secs(10));
        assert!(hooks.continue_section.is_visible(), "the in-progress book should keep the shelf alive");
        assert!(count_children(&hooks.recent_row) == 2, "the finished book still belongs under Recently Added");
    }

    /// The shelf headings' tap-through contract: each heading reports its shelf through
    /// `on_open_shelf` — the shell (not Home) owns the actual Library navigation.
    pub(crate) fn run_shelf_headers_invoke_on_open_shelf(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, "http://127.0.0.1:1"));

        let tapped: std::rc::Rc<std::cell::RefCell<Vec<Shelf>>> = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let tapped_for_closure = tapped.clone();
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, move |shelf: Shelf| tapped_for_closure.borrow_mut().push(shelf));
        let hooks = screen.test_hooks();

        hooks.continue_heading.emit_clicked();
        hooks.recent_heading.emit_clicked();

        assert_eq!(*tapped.borrow(), vec![Shelf::ContinueListening, Shelf::RecentlyAdded], "each heading reports its own shelf, in tap order");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A shelf is one Tab stop for a
    /// keyboard user: Tab from any of its cards leaves it, Shift+Tab goes back before it, and
    /// the cards say what they are.
    pub(crate) fn run_a_shelf_is_one_tab_stop(runtime: &tokio::runtime::Runtime) {
        use crate::a11y::tests::{description_of, focused_name, label_of, press_tab};
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary"), item_json("item-2", "Dune")]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();
        let window = gtk4::Window::builder().child(&screen.root).default_width(360).default_height(720).build();
        window.present();
        pump_until(|| count_children(&hooks.recent_row) == 2, Duration::from_secs(10));

        let first = hooks.recent_row.first_child().unwrap();
        assert_eq!(label_of(&first).as_deref(), Some("Project Hail Mary"), "a card is named after its book");
        assert!(description_of(&first).is_some_and(|text| text.contains("Andy Weir")), "with the author and length after it");

        first.grab_focus();
        assert!(press_tab(&hooks.recent_row, false), "Tab inside the shelf is handled by it");
        let after = GtkWindowExt::focus(&window).unwrap();
        assert!(!after.is_ancestor(&hooks.recent_row), "one Tab leaves the shelf, got {}", focused_name(&window));

        hooks.recent_row.last_child().unwrap().grab_focus();
        assert!(press_tab(&hooks.recent_row, true));
        assert_eq!(focused_name(&window), "Recently Added", "Shift+Tab from the shelf lands on its heading");
        window.close();
    }

    /// Toggling offline mode should narrow "Recently Added" to items with at least one completed
    /// download and show the banner; toggling off restores the full shelf.
    pub(crate) fn run_offline_mode_toggle_filters_recently_added(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary"), item_json("item-2", "Dune")]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();
        pump_until(|| count_children(&hooks.recent_row) == 2, Duration::from_secs(10));

        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-1", &[abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 3600.0, offset_seconds: 0.0, size_bytes: None }])).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::upsert_pending(&pool, &server.id, "item-1", "1", "/p/1.mp3")).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::mark_complete(&pool, &server.id, "item-1", "1", 10)).unwrap();

        // Same immediacy contract as `library.rs`'s offline scenario: the first toggle-on waits
        // for the handler's background refetch (the download above landed after this screen's
        // load, so the in-memory set is stale), but every later toggle below re-renders on the
        // next idle — no contended-pool wait, and not inside the toggle handler.
        hooks.offline_toggle.set_active(true);
        pump_until(|| count_children(&hooks.recent_row) == 1, Duration::from_secs(5));
        assert!(hooks.offline_banner.reveals_child(), "the offline banner should show while the toggle is active");

        // The toggle handler itself only flips cheap state, so the button redraws at once; the
        // shelves follow on the very next idle, from the in-memory set (no database wait).
        hooks.offline_toggle.set_active(false);
        assert!(!hooks.offline_banner.reveals_child());
        assert_eq!(count_children(&hooks.recent_row), 1, "the shelves aren't rebuilt inside the toggle handler");
        crate::test_support::run_pending_main_loop_work();
        assert_eq!(count_children(&hooks.recent_row), 2, "toggling offline mode off re-renders on the next idle");

        hooks.offline_toggle.set_active(true);
        crate::test_support::run_pending_main_loop_work();
        assert_eq!(count_children(&hooks.recent_row), 1, "toggling offline mode on filters on the next idle from the in-memory downloaded set");
    }

    pub(crate) fn run_shows_empty_state_when_the_server_has_no_libraries(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "libraries": [] })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        // An always-empty result looks identical before and after sync, but the state machine's
        // final resolution flips the retry button on — that's the observable "sync finished".
        pump_until(|| hooks.empty_state.retry.is_visible(), Duration::from_secs(10));

        assert!(hooks.empty_state.root.is_visible(), "no libraries at all should show the empty state");
        assert_eq!(hooks.empty_state.title.label(), "No library synced yet", "a server with zero libraries is not an error");
        assert!(!hooks.empty_state.details.is_visible(), "no error details for a genuinely empty server");
        assert!(!hooks.banner.widget().reveals_child(), "no failure banner for a genuinely empty server");
    }

    /// The first-sync failure path end to end. The original bug this guards against: the banner
    /// lived inside the scroller, which `apply` hides whenever nothing is cached — so a failed
    /// first sync revealed the banner into a hidden widget and the user saw only a static page.
    /// Now the failure gets its own retryable state, and a later successful retry (mock: 500
    /// once, then 200) clears it and lands the data.
    pub(crate) fn run_shows_a_retryable_error_when_the_first_sync_fails(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(500))
                .up_to_n_times(1)
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.empty_state.retry.is_visible(), Duration::from_secs(10));

        assert!(hooks.empty_state.root.is_visible(), "the failure state should replace the spinner");
        assert_eq!(hooks.empty_state.title.label(), "Couldn't sync your libraries");
        assert!(hooks.empty_state.details.is_visible(), "the underlying error should be shown for debugging");
        assert!(
            !hooks.banner.widget().reveals_child(),
            "the 'showing what's saved' banner must not appear when nothing is cached"
        );
        assert!(hooks.libraries_list.row_at_index(0).is_none());

        hooks.empty_state.retry.emit_clicked();
        // Wait on the state flip, not the rows: the cycle resolves the empty state only after
        // its (best-effort) cover-fetch stage, which finishes after the rows are already up.
        pump_until(|| !hooks.empty_state.root.is_visible(), Duration::from_secs(10));

        assert!(hooks.libraries_list.row_at_index(0).is_some(), "a successful retry should land the data");
        assert!(!hooks.banner.widget().reveals_child(), "a successful retry must not leave a failure banner up");
    }

    /// The manual sync success contract shared by the ⋯ menu's "Sync now" and the
    /// pull-to-refresh gesture (both feed the same manual path — ui-spec Home's "Sync now"):
    /// the mock's items response grows after the first sync consumes it (the established
    /// `up_to_n_times` idiom above), so the manual sync's round-trip is observable as the shelf
    /// gaining a card without leaving the tab — and the outcome must toast.
    fn run_manual_sync_lands_new_data_and_toasts(runtime: &tokio::runtime::Runtime, trigger: impl Fn(&TestHooks)) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .up_to_n_times(1)
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary"), item_json("item-2", "Dune")]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&screen.root));
        // The outcome toast only surfaces in a mapped overlay (`AdwToastOverlay` defers
        // unmapped toasts) — same mapped-window requirement as the shell/popover scenarios.
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        pump_until(|| count_children(&hooks.recent_row) == 1, Duration::from_secs(10));
        assert!(
            !crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Sync complete"),
            "the automatic cycle reports through the banner/empty state, not a toast"
        );

        trigger(hooks);
        // `ManualSync::claim` reveals the pull indicator synchronously, in the same handler
        // invocation as the trigger signal — no pump needed to observe it.
        assert!(hooks.pull_spinner.is_spinning(), "the pull indicator should reveal the instant a manual sync starts");

        pump_until(|| count_children(&hooks.recent_row) == 2, Duration::from_secs(10));
        // The toast lands at the cycle's resolve step — after its (best-effort) cover-fetch
        // stage, which can finish after the rows are already up — so wait on it, don't assert
        // it immediately.
        pump_until(
            || crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Sync complete"),
            Duration::from_secs(10),
        );
        assert!(
            crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Sync complete"),
            "the manual sync's completion toast must appear"
        );
        assert!(!hooks.pull_spinner.is_spinning(), "the pull indicator should retract once the cycle resolves");
    }

    /// "Sync now" via the ⋯ menu (HT-10): forces an immediate resync and toasts the outcome.
    pub(crate) fn run_sync_now_resyncs_and_toasts(runtime: &tokio::runtime::Runtime) {
        run_manual_sync_lands_new_data_and_toasts(runtime, |hooks| hooks.sync_now_button.emit_clicked());
    }

    /// Pull-to-refresh (HT-10's gesture twin): a pull past the main scroller's top runs the same
    /// manual sync path as the ⋯ menu. The sandbox has no touch hardware, so the scenario drives
    /// the scroller's own `edge-overshot` signal — the exact one the gesture listens to.
    pub(crate) fn run_pull_to_refresh_resyncs_and_toasts(runtime: &tokio::runtime::Runtime) {
        run_manual_sync_lands_new_data_and_toasts(runtime, |hooks| {
            hooks.scroller.emit_by_name::<()>("edge-overshot", &[&gtk4::PositionType::Top]);
        });
    }

    /// "Sync now" against a server nothing listens on: the toast reports the failure while the
    /// empty state stays the detailed failure surface — the toast deliberately carries no
    /// details, since that state (or the banner, with cached data) already shows them.
    pub(crate) fn run_sync_now_toasts_failure(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        // The unreachable-URL precedent from `run_shelf_headers_invoke_on_open_shelf`.
        let (server, account) = runtime.block_on(account_and_server(&pool, "http://127.0.0.1:1"));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&screen.root));
        // Mapped like the success scenarios — the failure toast needs the overlay mapped too.
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        // The first (automatic) sync fails into the error empty state; only then is a manual
        // re-trigger meaningful to press.
        pump_until(|| hooks.empty_state.retry.is_visible(), Duration::from_secs(10));

        hooks.sync_now_button.emit_clicked();
        pump_until(|| crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Sync failed"), Duration::from_secs(10));
        assert_eq!(
            hooks.empty_state.title.label(),
            "Can't reach your server",
            "the empty state stays the detailed failure surface"
        );
    }

    /// The authorization-failure path: the server 401s the sync (a dead session — revoked or
    /// expired refresh token, removed user). The state must not read as an ordinary sync failure:
    /// it offers "Log in again", and pressing it hands control to the shell (`on_relogin`) rather
    /// than pretending a retry could help.
    pub(crate) fn run_offers_login_again_when_the_session_is_rejected(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        // The stored token is a plain string (not a JWT), so `Session` hands it over untouched —
        // exactly the legacy-token case, and the request then 401s like a revoked session would.
        let relogin_requested = std::rc::Rc::new(std::cell::Cell::new(false));
        let on_relogin = {
            let relogin_requested = relogin_requested.clone();
            move || relogin_requested.set(true)
        };
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, on_relogin, |_| {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.empty_state.login_again.is_visible(), Duration::from_secs(10));

        assert!(hooks.empty_state.root.is_visible());
        assert_eq!(hooks.empty_state.title.label(), "Sign in again");
        assert!(hooks.empty_state.retry.is_visible(), "retry stays available alongside the login action");
        assert!(!hooks.banner.widget().reveals_child(), "nothing is cached, so no banner");
        assert!(!relogin_requested.get(), "merely showing the state must not trigger a re-login");

        hooks.empty_state.login_again.emit_clicked();
        assert!(
            relogin_requested.get(),
            "the login button must hand control back to the shell via on_relogin"
        );
    }

    /// Same dead session, but with cached data on screen: the failure lands in the banner (which
    /// stays up because the scroller no longer hides), with its own "Log in again" action.
    pub(crate) fn run_shows_login_again_in_the_banner_when_a_resync_is_rejected(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        // Data from a previous sync, so the shelves render and the failure must take the banner
        // path ("showing what's saved on this device"), not the empty-state path.
        runtime.block_on(abs_storage::repo::libraries::upsert(
            &pool,
            UpsertLibrary {
                id: "lib-1",
                server_id: &server.id,
                name: "Audiobooks",
                media_type: "book",
                icon: None,
                display_order: 1,
            },
        ))
        .unwrap();

        let relogin_requested = std::rc::Rc::new(std::cell::Cell::new(false));
        let on_relogin = {
            let relogin_requested = relogin_requested.clone();
            move || relogin_requested.set(true)
        };
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, on_relogin, |_| {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.banner.widget().reveals_child(), Duration::from_secs(10));

        assert_eq!(hooks.banner.title(), "Signed out by the server");
        assert!(hooks.banner.action_visible(), "an auth failure must offer Log in again in the banner");
        assert_eq!(hooks.banner.action_label(), "Log in again");
        assert!(!hooks.empty_state.root.is_visible(), "cached data keeps the shelves up; no empty state");

        hooks.banner.action_button().emit_clicked();
        assert!(relogin_requested.get(), "the banner's login action must fire on_relogin too");
    }

    /// A fresh login with a slow server: the spinner state must be up — and animated — for as
    /// long as the sync is in flight, instead of the old static "No library synced yet" page.
    /// The mock delay guarantees the sync is still running when the assertions fire.
    pub(crate) fn run_shows_a_spinner_while_the_first_sync_is_running(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({
                            "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                        }))
                        .set_delay(Duration::from_millis(400)),
                )
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary")]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        assert!(hooks.empty_state.root.is_visible(), "with nothing cached, the empty state should be up immediately");
        assert!(hooks.empty_state.spinner.is_visible(), "the spinner should be visible while the sync runs");
        assert!(hooks.empty_state.spinner.is_spinning(), "the spinner should be animated while the sync runs");
        assert!(!hooks.empty_state.retry.is_visible(), "there is nothing to retry while the sync is still in flight");

        pump_until(|| hooks.libraries_list.row_at_index(0).is_some(), Duration::from_secs(10));

        assert!(!hooks.empty_state.root.is_visible(), "the spinner state should clear once data lands");
        assert!(!hooks.banner.widget().reveals_child(), "a successful sync must not look like a failure");
    }

    /// The mid-session-expiry scenario that motivated `abs_core::auth::Session`: the stored access
    /// token is already expired when the screen's pipeline runs (the app was opened well after the
    /// last one died), and the server only accepts the token obtained via the refresh flow. The
    /// sync must go through — transparently — rather than surfacing as a 401 "logout".
    pub(crate) fn run_expired_token_is_refreshed_before_syncing(runtime: &tokio::runtime::Runtime) {
        use base64::Engine;

        let make_jwt = |exp: i64| {
            let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
            format!("{}.{}.{}", encode(br#"{"alg":"HS256"}"#), encode(serde_json::json!({ "exp": exp }).to_string().as_bytes()), encode(b"sig"))
        };

        let mock_server = runtime.block_on(MockServer::start());
        // Only the freshly-refreshed token gets data; the expired one (were it used) gets a 401.
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .and(wiremock::matchers::header("authorization", "Bearer brand-new-access"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("POST"))
                .and(path("/auth/refresh"))
                .and(wiremock::matchers::header("x-refresh-token", "stored-refresh"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "user": {
                        "id": "user-1",
                        "username": "jane",
                        "accessToken": "brand-new-access",
                        "refreshToken": "rotated-refresh",
                    }
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        // Re-record the account as it would look after the app sat closed past the token's life:
        // an expired JWT plus the refresh token to fix it with.
        runtime.block_on(abs_storage::repo::accounts::set_tokens(
            &pool,
            &account.id,
            &make_jwt(chrono::Utc::now().timestamp() - 60),
            Some("stored-refresh"),
        )).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account.id)).unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.libraries_list.row_at_index(0).is_some(), Duration::from_secs(10));

        assert!(hooks.libraries_list.row_at_index(0).is_some(), "syncing through the refreshed token should render the library");
        assert!(!hooks.banner.widget().reveals_child(), "a successful refresh must not look like a sync failure");
        assert!(!hooks.empty_state.root.is_visible(), "synced data should clear the empty state");
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(requests.iter().any(|r| r.url.path() == "/auth/refresh"), "the refresh endpoint should have been used");
    }

    /// End-to-end against the real public demo server, mirroring `welcome.rs`'s live test tier.
    /// Not a `#[test]` itself — run via `main.rs`'s consolidated `#[ignore]`d entry point.
    pub(crate) fn run_live(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(pool());
        let added = runtime
            .block_on(abs_core::accounts::add_server_and_login(
                &pool,
                "https://audiobooks.dev/audiobookshelf",
                "demo",
                "demo",
                &abs_core::passwords::NoPasswords,
                false,
            ))
            .expect("login against the live demo server should succeed");
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &added.server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &added.account_id)).unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.libraries_list.row_at_index(0).is_some(), Duration::from_secs(20));

        assert!(
            !hooks.empty_state.root.is_visible(),
            "the live demo server has at least one library, so the empty state should clear"
        );
        assert!(hooks.libraries_list.row_at_index(0).is_some());
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A book downloaded (or cleared) while
    /// the app runs gets (or loses) its badge on Home's shelves right away — it used to wait for
    /// the next sync, offline-mode toggle or restart.
    pub(crate) fn run_downloaded_badges_follow_downloads_in_this_session(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-1", "Project Hail Mary"), item_json("item-2", "Dune")]
                })))
                .mount(&mock_server),
        );
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account, session, offline_mode, |_| {}, || {}, |_| {});
        let download_manager =
            crate::downloads::DownloadManager::new(pool.clone(), crate::test_support::test_paths(), Box::new(abs_player::network_watch::UnknownNetworkMonitor), false);
        screen.follow_downloads(&download_manager);
        let hooks = screen.test_hooks();
        pump_until(|| count_children(&hooks.recent_row) == 2, Duration::from_secs(10));
        let badges = || {
            let mut shown = 0;
            let mut child = hooks.recent_row.first_child();
            while let Some(card) = child {
                if crate::widgets::item_card::tests::downloaded_badge_of(&card).is_visible() {
                    shown += 1;
                }
                child = card.next_sibling();
            }
            shown
        };
        assert_eq!(badges(), 0);

        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-1", &[abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 3600.0, offset_seconds: 0.0, size_bytes: None }])).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::upsert_pending(&pool, &server.id, "item-1", "1", "/p/1.mp3")).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::mark_complete(&pool, &server.id, "item-1", "1", 10)).unwrap();
        download_manager.publish_for_test(crate::downloads::DownloadEvent::ItemStateChanged { item_id: "item-1".to_string(), state: crate::downloads::ItemDownloadState::Complete });
        pump_until(|| badges() == 1, Duration::from_secs(5));
        assert_eq!(badges(), 1, "the finished download shows its badge without a sync");

        runtime.block_on(abs_storage::repo::download_tracks::remove_for_item(&pool, &server.id, "item-1")).unwrap();
        download_manager.publish_for_test(crate::downloads::DownloadEvent::ItemStateChanged { item_id: "item-1".to_string(), state: crate::downloads::ItemDownloadState::Idle });
        pump_until(|| badges() == 0, Duration::from_secs(5));
        assert_eq!(badges(), 0, "a cleared download loses its badge");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. With offline mode on, Home shows what's
    /// stored on the device and nothing reaches the server — not even a manual "Sync now", which
    /// says why instead.
    pub(crate) fn run_offline_mode_makes_no_requests(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        session.set_offline(true);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        let hooks = screen.test_hooks();
        pump_until(|| false, Duration::from_millis(800));

        hooks.sync_now_button.emit_clicked();
        pump_until(|| crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Offline mode is on — turn it off to sync"), Duration::from_secs(5));
        assert!(crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Offline mode is on — turn it off to sync"));
        pump_until(|| false, Duration::from_millis(300));
        let requests = runtime.block_on(mock_server.received_requests()).unwrap();
        assert!(requests.is_empty(), "offline mode: no request may reach the server, got {:?}", requests.iter().map(|r| r.url.path().to_string()).collect::<Vec<_>>());
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A cover the server doesn't have (404)
    /// isn't asked for again by the next automatic sync, but "Sync now" asks again.
    pub(crate) fn run_missing_covers_are_asked_again_only_by_a_manual_sync(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [item_json("item-1", "Project Hail Mary")] })))
                .mount(&mock_server),
        );
        runtime.block_on(Mock::given(method("GET")).and(path("/api/items/item-1/cover")).respond_with(ResponseTemplate::new(404)).mount(&mock_server));
        let pool = runtime.block_on(pool());
        let paths = crate::test_support::test_paths();
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let cover_requests = || {
            runtime.block_on(mock_server.received_requests()).unwrap().iter().filter(|r| r.url.path() == "/api/items/item-1/cover").count()
        };
        let build_home = || {
            let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
            let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
            build(pool.clone(), paths.clone(), server.clone(), account.clone(), session, offline_mode, |_| {}, || {}, |_| {})
        };

        let first = build_home();
        pump_until(|| cover_requests() == 1, Duration::from_secs(10));
        assert_eq!(cover_requests(), 1, "the first sync asks for the cover");
        pump_until(|| false, Duration::from_millis(500));

        // Another automatic sync (a new shell) doesn't ask again.
        let second = build_home();
        let hooks = second.test_hooks();
        pump_until(|| count_children(&hooks.recent_row) == 1, Duration::from_secs(10));
        pump_until(|| false, Duration::from_secs(1));
        assert_eq!(cover_requests(), 1, "a cover the server just said it doesn't have isn't asked for again");

        hooks.sync_now_button.emit_clicked();
        pump_until(|| cover_requests() == 2, Duration::from_secs(10));
        assert_eq!(cover_requests(), 2, "Sync now asks again");
        drop(first);
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Low memory mode turns the covers on
    /// the shelves into placeholders (and back), leaving everything else on the cards alone.
    pub(crate) fn run_low_memory_mode_hides_the_covers_on_the_shelves(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [item_json("item-1", "Project Hail Mary")] })))
                .mount(&mock_server),
        );
        let png = {
            let mut bytes = Vec::new();
            image::DynamicImage::new_rgb8(8, 8).write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
            bytes
        };
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/items/item-1/cover"))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "image/png").set_body_bytes(png))
                .mount(&mock_server),
        );
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let low_memory_mode = crate::low_memory_mode::LowMemoryModeState::new(pool.clone());
        low_memory_mode.add_listener(crate::widgets::cover_image::set_low_memory_mode);
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, |_| {});
        screen.follow_low_memory_mode(&low_memory_mode);
        let hooks = screen.test_hooks();
        let cover_showing = || {
            hooks
                .recent_row
                .first_child()
                .and_then(|card| crate::widgets::item_card::tests::cover_picture_of(&card))
                .is_some_and(|picture| picture.is_visible())
        };
        pump_until(cover_showing, Duration::from_secs(10));
        assert!(cover_showing(), "the cover shows normally");

        low_memory_mode.set(true);
        pump_until(|| !cover_showing(), Duration::from_secs(5));
        assert!(!cover_showing(), "low memory mode: a placeholder instead of the cover");
        assert_eq!(count_children(&hooks.recent_row), 1, "the card itself stays");

        low_memory_mode.set(false);
        pump_until(cover_showing, Duration::from_secs(10));
        assert!(cover_showing(), "and the cover comes back");
    }
}
