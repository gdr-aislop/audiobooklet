//! The Library tab's content — a searchable/sortable grid of every item across every synced
//! library, per `docs/design/ui-spec.md`'s "Library browse" section. Unlike Home's two curated
//! 10-item shelves, this shows everything.
//!
//! Category chips (All/Author/Series/Genre) and the view-options popover (Downloaded only/In
//! progress only/Hide finished/Grouping/Sort by/Application settings) both read and write
//! `abs_core::settings::LibraryViewOptions` — persisted, unlike the search box, which stays
//! session-only (see below). This popover used to coexist with an older "Sort & filter" popover;
//! that one has been folded into this sheet entirely (its two options with no equivalent here —
//! "In progress only" and a "Last listened" sort — were moved over, everything else already had
//! one) rather than kept as a second control. Grouped section headers (by author or series) are
//! *grouped*, not *sticky*: true sticky-while-scrolling headers need a `GtkListView`/`GListModel`-
//! bound render model, which this screen doesn't use anywhere (every render fully rebuilds
//! `flow_box`/`list_box` from scratch) — a documented simplification, not attempted here. Search
//! and sort are client-side over the already-synced local table — neither `abs-storage` nor the
//! real Audiobookshelf API surface this client uses expose search/sort/pagination query params,
//! so there is nothing server-side to delegate to yet.
//!
//! The grid/list choice is persisted via `abs_core::settings::{load,save}_library_view_mode` (a
//! plain key/value setting, same pattern as every other typed setting in that module) — restored
//! at startup and re-saved whenever the header's view toggle changes. Search text is session-only,
//! not persisted — the ui-spec doesn't ask for that, and a stale search filter silently narrowing
//! a freshly-opened Library screen would be a surprise, not a convenience. `apply_view` (Home's
//! Continue Listening tap-through) is likewise deliberately session-only even though it writes the
//! same `sort`/`in_progress_only` Cells the sheet's own persisted controls do — a temporary
//! navigation view, not the user setting a lasting preference.
//!
//! Search is debounced (`search_debounce` in `build()`) rather than re-rendering on every raw
//! `search-changed` emission, and every render's cards are built with their cover decode
//! deferred (`item_card::build_deferred`/`library_list_row_deferred`) rather than eager — only
//! covers within (or near) the scrolled viewport actually decode, tracked via `pending_covers`
//! and `decode_covers_in_viewport`, on scroll as well as right after a render. Both exist to fix
//! a reported freeze: typing used to re-render (and thus redecode every visible cover from disk,
//! synchronously) on every keystroke. `widgets::cover_image::CoverImage` itself now also decodes
//! asynchronously and caches decoded textures — see that module's doc comment for the rest of the
//! fix; this screen's half is just "don't ask for a cover before it's actually about to be seen."

use std::cell::Cell;
use std::rc::Rc;

use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use abs_core::error::{CoreError, Result as CoreResult};
use abs_core::settings::{Grouping, LibraryViewMode, LibraryViewOptions, SortBy};
use abs_storage::models::{Account, Item, Progress, Server};
use abs_storage::AppPaths;

use crate::i18n::{ntr, tr, tr_args, tr_ctx};
use crate::player::PlayRequest;
use crate::widgets::{combo_row, item_card};

// Small enough that at least 2 columns fit at this app's default phone width (390px, see
// `application.rs`) once the flat button's own padding and the `GtkFlowBoxChild` wrapper's
// intrinsic padding are added on top of the raw cover size — confirmed live: 140 rendered as a
// single column with a lot of unused horizontal space, since the button+wrapper overhead alone was
// enough to push the cell's natural width just past half the available content width.
const TILE_SIZE: i32 = 108;

/// The view-options button's icon: always the funnel — the button opens the sort and filter
/// options. (It used to show `preferences-other-symbolic`, a toolbox in some themes, and switch
/// to the funnel only while a filter was on.) A filter being on now shows as the funnel in the
/// accent colour — see [`update_view_options_indicator`].
const VIEW_OPTIONS_ICON: &str = "funnel-symbolic";

#[derive(Clone)]
pub struct LibraryScreen {
    pub root: gtk4::Widget,
    /// Public so the main window's `win.open-library-search` keyboard action (ui-spec §6) can
    /// focus it — `TestHooks` is test-only by convention, and this is production wiring, not a
    /// test hook.
    pub search_entry: gtk4::SearchEntry,
    /// Kept on the screen so navigation-with-intent can land here pre-sorted/pre-filtered —
    /// Home's shelf headers call [`Self::apply_view`] through the shell. Same shared-handle
    /// posture as every other widget field: `LibraryWidgets` is cheap-clone, and the async
    /// pipeline already holds its own clone.
    widgets: LibraryWidgets,
    /// What `follow_downloads` needs to re-read which books are downloaded.
    pool: SqlitePool,
    server_id: String,
    #[cfg(test)]
    hooks: TestHooks,
}

impl LibraryScreen {
    /// Re-renders the list when low memory mode changes, so the covers already shown turn into
    /// placeholders (or come back).
    pub(crate) fn follow_low_memory_mode(&self, low_memory_mode: &crate::low_memory_mode::LowMemoryModeState) {
        let widgets = self.widgets.clone();
        low_memory_mode.add_listener(move |_| {
            // The books are the same, so the "nothing changed" check would skip the rebuild; the
            // new cards are what drop the old covers.
            *widgets.rendered_signature.borrow_mut() = None;
            request_render(&widgets);
        });
    }

    /// Keeps the cards' downloaded badges (and the offline-mode view) current as downloads finish
    /// or are cleared — see `crate::downloads::follow_downloaded_items`. An unchanged list isn't
    /// rebuilt (the render compares what it would show with what is shown).
    pub(crate) fn follow_downloads(&self, download_manager: &crate::downloads::DownloadManager) {
        let widgets = self.widgets.clone();
        crate::downloads::follow_downloaded_items(download_manager, self.pool.clone(), self.server_id.clone(), self.root.downgrade(), move |downloaded| {
            {
                let mut data = widgets.data.borrow_mut();
                if data.downloaded == downloaded {
                    return;
                }
                tracing::debug!(downloaded = downloaded.len(), "downloaded books changed; refreshing Library's badges");
                data.downloaded = downloaded;
            }
            render_from_current_data(&widgets);
        });
    }

    /// Navigation-with-intent entry point (docs/design/ui-spec.md, Home tap-through): Home's
    /// shelf headers switch to this screen pre-sorted — and, for Continue Listening,
    /// pre-filtered to in-progress books — without persisting anything, exactly like a manual
    /// sort/filter change (both are session-transient by the same reasoning: a view that
    /// silently hides books across restarts is a trap, not a convenience).
    pub(crate) fn apply_view(&self, key: SortKey, in_progress_only: bool) {
        self.widgets.sort.set(key);
        // `set_in_progress_only` re-renders with both the new sort and the new filter.
        set_in_progress_only(&self.widgets, in_progress_only);
    }

    /// Item Detail's series-button tap-through (docs/design/ui-spec.md): filters to exactly one
    /// series by name, same session-only navigation-with-intent contract as `apply_view` — no
    /// persistence, and no grouping headers (`Grouping::None`), since this is a single-series
    /// view, not a grouped one. None of the four category chips shows as "active" while this
    /// filter is applied — the same acceptable gap `apply_view` already has (no sort button
    /// highlights after Home's tap-through either).
    pub(crate) fn apply_series_filter(&self, series_name: &str) {
        *self.widgets.category_filter.borrow_mut() = CategoryFilter::OneSeries(series_name.to_string());
        self.widgets.grouping.set(Grouping::None);
        request_render(&self.widgets);
    }
}

#[cfg(test)]
#[derive(Clone)]
pub struct TestHooks {
    pub status_page: adw::StatusPage,
    pub flow_box: gtk4::FlowBox,
    pub list_box: gtk4::ListBox,
    pub search_entry: gtk4::SearchEntry,
    pub banner: crate::widgets::banner::ErrorBanner,
    pub progress_banner: gtk4::Revealer,
    pub progress_show_all: gtk4::Button,
    pub view_toggle: gtk4::ToggleButton,
    pub offline_toggle: gtk4::ToggleButton,
    pub offline_banner: gtk4::Revealer,
    pub sync_now_button: gtk4::Button,
    pub toast_overlay: adw::ToastOverlay,
    pub scroller: gtk4::ScrolledWindow,
    pub view_options_button: gtk4::MenuButton,
    pub view_options_popover: gtk4::Popover,
    pub downloaded_only_switch: gtk4::Switch,
    pub in_progress_only_switch: gtk4::Switch,
    pub hide_finished_switch: gtk4::Switch,
    pub grouping_row: adw::ComboRow,
    pub sort_by_row: adw::ComboRow,
    pub settings_row: adw::ActionRow,
    pub category_all: gtk4::ToggleButton,
    pub category_author: gtk4::ToggleButton,
    pub category_series: gtk4::ToggleButton,
    pub category_genre: gtk4::ToggleButton,
    pub genre_chip_box: gtk4::Box,
    pub genre_chip_revealer: gtk4::Revealer,
    pub busy_spinner: gtk4::Spinner,
    pub busy_scrim: gtk4::Box,
    pub pull_spinner: gtk4::Spinner,
}

#[cfg(test)]
impl LibraryScreen {
    pub fn test_hooks(&self) -> &TestHooks {
        &self.hooks
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SortKey {
    DateAdded,
    Title,
    Author,
    Duration,
    /// Not a metadata field but a playback one: items with a progress row first, newest
    /// last-listen first (`progress.updated_at` — the server's own last-update time on import),
    /// never-played items after them in stable order. Backs Home's "Continue Listening"
    /// tap-through and the popover's "Last listened" entry.
    LastListened,
}

impl SortKey {
    /// `SortKey` and `abs_core::settings::SortBy` are a straight bijection — one variant per
    /// value, `LastListened` included — so the view-options sheet's persisted "Sort by" combo can
    /// express every sort this screen supports.
    fn from_sort_by(sort_by: SortBy) -> Self {
        match sort_by {
            SortBy::DateOfCreation => SortKey::DateAdded,
            SortBy::Title => SortKey::Title,
            SortBy::Author => SortKey::Author,
            SortBy::Duration => SortKey::Duration,
            SortBy::LastListened => SortKey::LastListened,
        }
    }

    fn to_sort_by(self) -> SortBy {
        match self {
            SortKey::DateAdded => SortBy::DateOfCreation,
            SortKey::Title => SortBy::Title,
            SortKey::Author => SortBy::Author,
            SortKey::Duration => SortBy::Duration,
            SortKey::LastListened => SortBy::LastListened,
        }
    }
}

/// Which category chip is active (ui-spec "Library browse": All/Author/Series/Genre). Author and
/// Series just mirror the persisted `Grouping` choice (see [`sync_grouping_and_category`]); Genre
/// is a session-only filter — there's no `Grouping::ByGenre` (a book has several genres, not one
/// grouping-per-genre) and no genre field in `LibraryViewOptions`. `OneSeries` is a third, distinct
/// mode with no chip of its own: Item Detail's series-button tap-through
/// (`LibraryScreen::apply_series_filter`) filters to exactly one series by name, unlike the bare
/// `Series` chip above (which only groups, showing every series' books under a header each).
#[derive(Clone, Debug, Default, PartialEq)]
enum CategoryFilter {
    #[default]
    All,
    Author,
    Series,
    Genre(String),
    OneSeries(String),
}

/// One render's worth of built-but-maybe-not-decoded card/row, tracked so
/// `decode_covers_in_viewport` can look them up again later (on scroll) without re-deriving
/// anything from `data`. Rebuilt fresh on every render, replacing whatever was tracked before —
/// the previous render's cards/covers are dropped along with their widgets, same lifetime as
/// before this existed.
struct PendingCover {
    widget: gtk4::Widget,
    cover: crate::widgets::cover_image::CoverImage,
    path: Option<std::path::PathBuf>,
}

/// Everything `render` needs a handle to, cloned as a whole into the `spawn_future_local` block —
/// every field is a reference-counted GTK/Adwaita widget handle, so cloning is cheap and shares
/// the same underlying widgets, not copies. Mirrors `home.rs`'s `HomeWidgets` shape.
#[derive(Clone)]
struct LibraryWidgets {
    status_page: adw::StatusPage,
    scroller: gtk4::ScrolledWindow,
    flow_box: gtk4::FlowBox,
    list_box: gtk4::ListBox,
    banner: crate::widgets::banner::ErrorBanner,
    search_entry: gtk4::SearchEntry,
    sort: Rc<Cell<SortKey>>,
    /// The one knob behind every "in progress only" surface — the sheet's switch, the view-options
    /// button's filter-active highlight, and the in-view banner all read/write it through
    /// [`set_in_progress_only`] so they can't drift apart. Persisted via the sheet's own switch
    /// handler (see [`spawn_persist_view_options`]) — but `apply_view`'s Home tap-through writes
    /// this Cell directly, without persisting, since that's a temporary navigation view rather
    /// than the user setting a lasting preference.
    in_progress_only: Rc<Cell<bool>>,
    view_mode: Rc<Cell<LibraryViewMode>>,
    /// Persisted — the view-options sheet's "Hide finished" switch and "Grouping" combo, plus the
    /// category chip row, read/write these two alongside `sort`/`in_progress_only` through
    /// [`spawn_persist_view_options`].
    hide_finished: Rc<Cell<bool>>,
    grouping: Rc<Cell<Grouping>>,
    /// `RefCell`, not `Cell`, because `CategoryFilter::Genre` carries a `String` — not `Copy`,
    /// so `Cell::get` isn't available for it.
    category_filter: Rc<std::cell::RefCell<CategoryFilter>>,
    /// Shared, live-updating state per `docs/design/ui-spec.md` ("not a per-screen setting") —
    /// see `crate::offline_mode::OfflineModeState`'s doc for why this can't be a screen-local
    /// `Cell` (that was the actual bug: toggling on one screen never reached the other's
    /// already-built instance).
    offline_mode: crate::offline_mode::OfflineModeState,
    data: Rc<std::cell::RefCell<LibraryData>>,
    on_open: Rc<dyn Fn(PlayRequest)>,
    /// The view-options button — mutated only to reflect whether any filter (in-progress-only,
    /// hide-finished, downloaded-only) is active (its funnel in the accent colour), never to *own*
    /// any of them. See [`update_view_options_indicator`].
    view_options_button: gtk4::MenuButton,
    in_progress_only_switch: gtk4::Switch,
    progress_banner: gtk4::Revealer,
    /// Rebuilt from scratch (`rebuild_genre_chips`) whenever `data` reloads; shown only while the
    /// Genre category chip is active. The revealer that shows/hides this box is only ever driven
    /// from the chip handlers in `build()` (which already hold their own clone), not from here.
    genre_chip_box: gtk4::Box,
    /// This render's cards/rows and their (maybe not yet decoded) covers — see [`PendingCover`].
    pending_covers: Rc<std::cell::RefCell<Vec<PendingCover>>>,
    /// Bumped by every render that rebuilds the list; a slice still building an older one sees the
    /// change and stops (`append_remaining_entries`).
    render_generation: Rc<Cell<u64>>,
    /// What the fully built list on screen was built from (`render_signature`); `None` while a
    /// render is still in progress or nothing has been rendered.
    rendered_signature: Rc<std::cell::RefCell<Option<String>>>,
    /// Shown for the one main-loop tick between any filter/search/sort/view-mode change and the
    /// rebuild it triggers — see [`request_render`]/[`set_busy`].
    busy_spinner: gtk4::Spinner,
    /// The dimming layer behind `busy_spinner`, sized to cover the *entire* content area (not
    /// just the spinner's own small box) — see `busy_spinner`'s construction comment in `build()`
    /// for why a bare spinner over arbitrary list/grid content needs this at all.
    busy_scrim: gtk4::Box,
}

struct LibraryData {
    items: Vec<Item>,
    /// Ids of items downloaded fully or partially — loaded once alongside `items` (one query, not
    /// one per card). Feeds `item_card::build`'s read-only download badge.
    downloaded: std::collections::HashSet<String>,
    /// Every progress row for the account, keyed by item id — backs the `LastListened` sort and
    /// the "In progress only" filter. Loaded in the same one-query-per-concern pass as
    /// `downloaded`, never per card.
    last_listened: std::collections::HashMap<String, Progress>,
}

/// Builds the screen. Signature mirrors `home::build`'s exactly — same reasoning: `server`/
/// `account` are already-resolved rows the caller looks up once, and `on_open` is how tapping a
/// cover starts playback without this screen ever touching `abs-player`/`abs-core::streaming`
/// itself. `on_relogin` routes the banner's "Log in again" action (authorization failures only)
/// back to the shell, same as Home's.
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
    on_open_settings: impl Fn() + Clone + 'static,
) -> LibraryScreen {
    let header = adw::HeaderBar::new();

    // A persistent, always-visible search entry — not revealed behind a search button — per the
    // ui-spec's explicit reasoning: search is high-frequency in a large library, worth the
    // permanent header-bar space on a device where reveal-then-tap-then-type is already awkward
    // one-handed.
    let search_entry = gtk4::SearchEntry::builder().hexpand(true).placeholder_text(tr("Search library")).build();
    header.set_title_widget(Some(&search_entry));

    // The filter's ambient indicator while the sheet is closed (the highlighted funnel on the
    // view-options button below is only a hint) — an in-view "why are books hidden" banner with a
    // one-tap escape,
    // built from the same banner row as the offline banner below. `ErrorBanner` is wrong here:
    // this is a neutral filter notice, not a failure.
    // Shared by both filter notices (this one and the offline banner below) so the two can't
    // drift apart — the caption label gets its own lightly-margined row rather than hugging the
    // screen edge. This banner alone appends its "Show all" escape button to the returned row.
    let caption_banner_row = |text: &str| {
        let label = gtk4::Label::builder().label(text).xalign(0.0).hexpand(true).css_classes(["caption", "dim-label"]).build();
        let row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(8).margin_start(12).margin_end(12).margin_top(4).margin_bottom(4).build();
        row.append(&label);
        (label, row)
    };
    let (_, progress_banner_row) = caption_banner_row(&tr("Showing books in progress"));
    let progress_show_all = gtk4::Button::builder().label(&tr("Show all")).css_classes(["flat"]).valign(gtk4::Align::Center).build();
    progress_banner_row.append(&progress_show_all);
    let progress_banner = gtk4::Revealer::builder().transition_type(gtk4::RevealerTransitionType::SlideDown).child(&progress_banner_row).reveal_child(false).build();

    // Grid/list toggle — a plain header-bar button per the ui-spec's own wording ("toggle between
    // the two via header bar button"), separate from the view-options popover below. Starts
    // active, showing the "switch to grid" icon, since List is the default mode.
    let view_toggle = gtk4::ToggleButton::builder().active(true).icon_name("view-grid-symbolic").tooltip_text(tr("Grid view")).build();
    // For a screen reader it's a switch named for the mode it's in when on (checked = list),
    // where the tooltip names the mode a tap switches to.
    crate::a11y::switch_like(&view_toggle);
    crate::a11y::name(&view_toggle, &tr("List view"));
    header.pack_end(&view_toggle);

    // The view-options popover (ui-spec "Library browse": Downloaded only/In progress only/Hide
    // finished/Grouping/Sort by/Application settings) — same "popover approximates
    // `AdwBottomSheet`" convention `widgets::download_scope_menu` already uses (this crate's
    // libadwaita ceiling is `v1_2`; `AdwBottomSheet` needs 1.6). `AdwSwitchRow` also needs 1.4
    // (out of reach), so the switch rows use the same `AdwActionRow` + `gtk4::Switch`
    // substitution `screens::settings`'s headphone-behavior rows already establish. Consolidates
    // what used to be a separate "Sort & filter" popover — "In progress only" and "Last listened"
    // (below) were that popover's only two entries with no equivalent here; everything else it
    // offered already existed on this sheet.
    let downloaded_only_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    let downloaded_only_row = adw::ActionRow::builder().title(tr("Downloaded only")).build();
    downloaded_only_row.add_suffix(&downloaded_only_switch);
    crate::a11y::switch_row(&downloaded_only_row, &downloaded_only_switch);

    // The one manual home of the "In progress only" filter — the same state Home's Continue
    // Listening header sets on tap-through (see `apply_view`). All state surfaces (this switch,
    // the banner below, the view-options button's indicator) funnel through
    // `set_in_progress_only`. Unlike Home's tap-through, changing this switch persists (see its
    // `connect_state_set` handler below).
    let in_progress_only_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    let in_progress_only_row = adw::ActionRow::builder().title(tr("In progress only")).build();
    in_progress_only_row.add_suffix(&in_progress_only_switch);
    crate::a11y::switch_row(&in_progress_only_row, &in_progress_only_switch);

    let hide_finished_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    let hide_finished_row = adw::ActionRow::builder().title(tr("Hide finished")).build();
    hide_finished_row.add_suffix(&hide_finished_switch);
    crate::a11y::switch_row(&hide_finished_row, &hide_finished_switch);

    let grouping_row = combo_row(&tr("Grouping"), "", &[tr("None"), tr("By Series"), tr("By Author")]);
    let sort_by_row = combo_row(
        &tr("Sort by"),
        "",
        &[tr("Date of creation"), tr("Title"), tr("Author"), tr("Duration"), tr("Last listened")],
    );

    let settings_row = adw::ActionRow::builder().title(tr("Application settings")).activatable(true).build();
    settings_row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));

    // A `GtkListBox` (the "boxed list" convention `settings.rs`'s own preference groups use),
    // not a plain `GtkBox` — `AdwActionRow`/`AdwComboRow` (every row here) expect a `GtkListBox`
    // ancestor for their internal focus/activation handling; without one, activating a row logs
    // a `gtk_list_box_row_grab_focus: assertion 'box != NULL' failed` critical.
    let view_options_box = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).width_request(260).build();
    view_options_box.append(&downloaded_only_row);
    view_options_box.append(&in_progress_only_row);
    view_options_box.append(&hide_finished_row);
    view_options_box.append(&grouping_row);
    view_options_box.append(&sort_by_row);
    view_options_box.append(&settings_row);
    let view_options_popover = gtk4::Popover::builder().child(&view_options_box).build();
    let view_options_button = gtk4::MenuButton::builder().icon_name(VIEW_OPTIONS_ICON).tooltip_text(tr("View options")).popover(&view_options_popover).build();
    crate::a11y::name(&view_options_button, &tr("View options"));
    header.pack_end(&view_options_button);

    // Offline-mode toggle (ui-spec: "leading side, opposite the avatar" on Home; mirrored here on
    // the leading side too, alongside the search entry). Shared persisted state with Home's own
    // toggle, not a per-screen setting — see `offline_mode`'s field doc.
    let offline_toggle = gtk4::ToggleButton::builder().icon_name("airplane-mode-symbolic").tooltip_text(tr("Offline mode")).build();
    crate::a11y::switch_like(&offline_toggle);
    crate::a11y::name(&offline_toggle, &tr("Offline mode"));
    header.pack_start(&offline_toggle);

    // Category chips (ui-spec "Library browse": All/Author/Series/Genre), a horizontally
    // scrolling row below the header, independent of the main scroller. Author/Series just set
    // `grouping` — the same persisted value the popover's own combo drives, via [`set_grouping`]
    // — mirroring exactly what the combo does; Genre reveals a second row of the distinct genre
    // values currently loaded (see [`rebuild_genre_chips`]) rather than grouping, since a book
    // has several genres, not one grouping-per-genre (see [`CategoryFilter`]'s doc comment).
    let category_all = gtk4::ToggleButton::builder().label(tr("All")).active(true).build();
    let category_author = gtk4::ToggleButton::builder().label(tr("Author")).group(&category_all).build();
    let category_series = gtk4::ToggleButton::builder().label(tr("Series")).group(&category_all).build();
    let category_genre = gtk4::ToggleButton::builder().label(tr("Genre")).group(&category_all).build();
    let category_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).margin_start(12).margin_end(12).margin_top(6).margin_bottom(4).build();
    for chip in [&category_all, &category_author, &category_series, &category_genre] {
        // One choice of four: radio buttons, so a screen reader says which is active.
        crate::a11y::radio(chip);
        category_row.append(chip);
    }
    // One Tab stop for the row; Left/Right move between chips. The row is also where a screen
    // reader lands on this tab (the header's title widget is the search entry, which must not
    // take focus by itself: that would open the on-screen keyboard); Tab then enters the chips.
    crate::a11y::focus_group(&category_row);
    crate::a11y::landing(&category_row, &tr("Library"));
    let category_scroller = gtk4::ScrolledWindow::builder().hscrollbar_policy(gtk4::PolicyType::Automatic).vscrollbar_policy(gtk4::PolicyType::Never).child(&category_row).build();

    // Populated fresh from `data.items` whenever it reloads (`rebuild_genre_chips`), same
    // full-rebuild convention as `flow_box`/`list_box`. Hidden until the Genre chip is active.
    let genre_chip_box = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).margin_start(12).margin_end(12).margin_bottom(6).build();
    crate::a11y::focus_group(&genre_chip_box);
    let genre_chip_scroller = gtk4::ScrolledWindow::builder().hscrollbar_policy(gtk4::PolicyType::Automatic).vscrollbar_policy(gtk4::PolicyType::Never).child(&genre_chip_box).build();
    let genre_chip_revealer = gtk4::Revealer::builder().transition_type(gtk4::RevealerTransitionType::SlideDown).child(&genre_chip_scroller).reveal_child(false).build();

    // "Sync now" (ui-spec Library browse) — the same header-bar ⋯ overflow as Home's, forcing
    // an immediate resync; the pull-to-refresh gesture on the main scroller (wired below) runs
    // the exact same manual path. Plain `GtkMenuButton` + flat button, per this crate's
    // no-GMenu convention.
    let sync_now_button = gtk4::Button::builder().label(tr("Sync now")).css_classes(["flat"]).build();
    let sync_menu_popover = gtk4::Popover::builder().child(&sync_now_button).build();
    let sync_menu_button = gtk4::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text(tr("More"))
        .popover(&sync_menu_popover)
        .build();
    crate::a11y::name(&sync_menu_button, &tr("More"));
    header.pack_end(&sync_menu_button);

    let (_, offline_banner_row) = caption_banner_row(&tr("Showing downloaded items only"));
    let offline_banner = gtk4::Revealer::builder().transition_type(gtk4::RevealerTransitionType::SlideDown).child(&offline_banner_row).reveal_child(false).build();

    let banner = crate::widgets::banner::ErrorBanner::new();

    // Hidden until the user switches to Grid mode — List is the default (see `LibraryViewMode`).
    let flow_box = gtk4::FlowBox::builder()
        .homogeneous(true)
        .column_spacing(8)
        .row_spacing(12)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(16)
        .selection_mode(gtk4::SelectionMode::None)
        .valign(gtk4::Align::Start)
        .visible(false)
        .build();

    // List mode: a plain `GtkListBox` of `AdwActionRow`s (same "boxed list" pattern `home.rs`'s
    // `libraries_list` already uses), not the spec's literal `GtkListView` — this codebase already
    // substitutes a simpler widget for the grid too (`GtkFlowBox`, not `GtkGridView`). List is the
    // default mode, so this starts visible.
    let list_box = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(16)
        .margin_end(16)
        .margin_top(12)
        .margin_bottom(16)
        .build();

    let scroll_content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    scroll_content.append(&flow_box);
    scroll_content.append(&list_box);

    let scroller = gtk4::ScrolledWindow::builder().hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).child(&scroll_content).build();
    // The chip rows sit above the list, not in it: the mouse wheel over them scrolls the list.
    crate::widgets::sideways_scroll::pass_vertical_scroll_to(&category_scroller, &scroller);
    crate::widgets::sideways_scroll::pass_vertical_scroll_to(&genre_chip_scroller, &scroller);

    // Immediate feedback for any filter/search/sort/view-mode change (see `request_render`/
    // `apply_view_mode`): the rebuild that follows can take a couple of seconds on a large
    // library, so this spinner is shown on the very next painted frame, before that rebuild ever
    // runs, rather than leaving the screen looking unresponsive in the meantime. Overlaid rather
    // than swapped in so the stale grid/list stays visible underneath instead of vanishing to
    // blank.
    //
    // A bare `GtkSpinner` renders as a handful of thin, foreground-colored dashes with no
    // background of its own, sized to its own small natural size — over this screen's own
    // (typically light) content, in the middle of an otherwise untouched list, that's easy to
    // miss entirely: exactly the "I wouldn't notice it if I didn't know where to look" report
    // this exists to fix. `busy_scrim` is a full-bleed dark dimming layer, sized to the *whole*
    // content area (it has no `halign`/`valign`, so `GtkOverlay` sizes it to match the main
    // child, unlike the spinner centered on top of it) — the same "dim what's behind, show a
    // spinner on top" shape a modal loading state uses elsewhere, rather than a small backdrop
    // sized to the spinner alone, which would still be easy to miss against a large list.
    ensure_busy_overlay_css();
    let busy_scrim = gtk4::Box::builder().hexpand(true).vexpand(true).css_classes(["library-busy-scrim"]).visible(false).build();
    let busy_spinner = gtk4::Spinner::builder()
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .width_request(56)
        .height_request(56)
        .css_classes(["library-busy-spinner"])
        .visible(false)
        .build();
    let content_overlay = gtk4::Overlay::builder().child(&scroller).build();
    content_overlay.add_overlay(&busy_scrim);
    content_overlay.add_overlay(&busy_spinner);

    let status_page = adw::StatusPage::builder()
        .icon_name("folder-music-symbolic")
        .title(tr("No items yet"))
        .description(tr("Check your connection and try again."))
        .vexpand(true)
        .visible(false)
        .build();

    // The banners live directly under the header bar, outside the scroller — same reason as
    // home.rs's identically-placed banners: `apply` hides the scroller whenever nothing is
    // cached (zero items renders the status page instead), and a banner trapped inside the
    // hidden scroller disappears without a trace. Above the status page too, so the failure
    // is visible in every state.
    let (pull_indicator_widget, pull_indicator) = crate::widgets::pull_to_refresh::PullIndicator::build();
    let body = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    body.append(&offline_banner);
    body.append(&progress_banner);
    body.append(banner.widget());
    body.append(&pull_indicator_widget);
    body.append(&content_overlay);
    body.append(&status_page);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&category_scroller);
    root.append(&genre_chip_revealer);
    root.append(&body);

    // Toasts float over the whole screen, header bar included — same shape as the player
    // screen's and Home's own overlays. Manual sync triggers report through this one.
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&root));
    crate::a11y::focus_landing(&category_row, &toast_overlay);

    let widgets = LibraryWidgets {
        status_page: status_page.clone(),
        scroller: scroller.clone(),
        flow_box: flow_box.clone(),
        list_box: list_box.clone(),
        banner: banner.clone(),
        search_entry: search_entry.clone(),
        sort: Rc::new(Cell::new(SortKey::DateAdded)),
        in_progress_only: Rc::new(Cell::new(false)),
        view_mode: Rc::new(Cell::new(LibraryViewMode::List)),
        hide_finished: Rc::new(Cell::new(false)),
        grouping: Rc::new(Cell::new(Grouping::None)),
        category_filter: Rc::new(std::cell::RefCell::new(CategoryFilter::All)),
        offline_mode,
        data: Rc::new(std::cell::RefCell::new(LibraryData { items: Vec::new(), downloaded: std::collections::HashSet::new(), last_listened: std::collections::HashMap::new() })),
        on_open: Rc::new(on_open),
        view_options_button: view_options_button.clone(),
        in_progress_only_switch: in_progress_only_switch.clone(),
        progress_banner: progress_banner.clone(),
        genre_chip_box: genre_chip_box.clone(),
        pending_covers: Rc::new(std::cell::RefCell::new(Vec::new())),
        render_generation: Rc::new(Cell::new(0)),
        rendered_signature: Rc::new(std::cell::RefCell::new(None)),
        busy_spinner: busy_spinner.clone(),
        busy_scrim: busy_scrim.clone(),
    };

    // The filter's two manual entry points that don't persist: the banner's "Show all", and (via
    // `apply_view`) Home's Continue Listening tap-through. Both funnel through
    // `set_in_progress_only`; no signal-blocking is needed when it writes the switch back —
    // `set_active` to the value it already holds doesn't re-emit `state-set`, so the write-back
    // terminates immediately (and re-syncs rather than fights). The sheet's own switch (which
    // does persist) is wired separately, below, alongside `hide_finished_switch`.
    progress_show_all.connect_clicked({
        let widgets = widgets.clone();
        move |_| set_in_progress_only(&widgets, false)
    });

    // Debounced (200ms, cancel-and-reschedule) rather than rendering on every raw
    // `search-changed` emission — a full render rebuilds every visible card/row, and firing that
    // on every keystroke (even with `GtkSearchEntry`'s own ~150ms internal coalescing) was the
    // actual cause of a reported freeze-then-catch-up pattern while typing. Only the *last*
    // keystroke within a quiet window ever triggers a render, so no stale intermediate query's
    // results can flash on screen either. `crate::widgets::Debouncer`, not a hand-rolled
    // `SourceId` cell — see its doc comment for why a hand-rolled version of this exact thing
    // crashed the app.
    let search_debounce = crate::widgets::Debouncer::default();
    search_entry.connect_search_changed({
        let widgets = widgets.clone();
        move |_| {
            let widgets = widgets.clone();
            search_debounce.schedule(std::time::Duration::from_millis(200), move || {
                request_render(&widgets);
            });
        }
    });

    // Debounced (100ms) recompute of which pending covers just scrolled into range — see
    // `decode_covers_in_viewport`'s doc comment. Scroll events fire far more often than a search
    // keystroke, so a shorter debounce than search's own is enough to avoid redundant recomputes
    // without adding perceptible lag to when a newly-visible cover starts decoding.
    let scroll_debounce = crate::widgets::Debouncer::default();
    widgets.scroller.vadjustment().connect_value_changed({
        let widgets = widgets.clone();
        move |_| {
            let widgets = widgets.clone();
            scroll_debounce.schedule(std::time::Duration::from_millis(100), move || {
                decode_covers_in_viewport(&widgets);
            });
        }
    });

    let view_toggle_handler = view_toggle.connect_toggled({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let toast_overlay = toast_overlay.clone();
        move |toggle| {
            let mode = if toggle.is_active() { LibraryViewMode::List } else { LibraryViewMode::Grid };
            apply_view_mode(mode, &widgets, toggle);
            glib::spawn_future_local({
                let pool = pool.clone();
                let toast_overlay = toast_overlay.clone();
                async move {
                    if let Err(err) = abs_core::settings::save_library_view_mode(&pool, mode).await {
                        crate::error_reporting::report_background_error(&toast_overlay, &tr("Saving view mode"), err);
                    }
                }
            });
        }
    });

    // Loaded once, asynchronously, right after construction (a local-only DB read, no network).
    // The toggle's own handler is blocked while this sets the *initial* `active` state — without
    // that, restoring a persisted List mode would itself fire `connect_toggled`, which would
    // immediately re-save the exact value it just loaded (harmless, but a pointless write on every
    // single launch) and — worse, if a future edit made that handler do anything not idempotent —
    // a real bug waiting to happen.
    glib::spawn_future_local({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let view_toggle = view_toggle.clone();
        async move {
            if let Ok(mode) = abs_core::settings::load_library_view_mode(&pool).await {
                view_toggle.block_signal(&view_toggle_handler);
                view_toggle.set_active(mode == LibraryViewMode::List);
                view_toggle.unblock_signal(&view_toggle_handler);
                apply_view_mode(mode, &widgets, &view_toggle);
            }
        }
    });

    let offline_toggle_server_id = server.id.clone();

    // Shrunk to just forwarding into the shared state — every visible effect (banner, re-render,
    // background refetch, persistence, and this screen's own `LibraryViewOptions.downloaded_only`
    // sync) now lives in the `add_listener` callback below, since that same logic must run
    // whether *this* screen's own toggle fired or Home's did.
    let offline_toggle_handler = offline_toggle.connect_toggled({
        let offline_mode = widgets.offline_mode.clone();
        move |toggle| {
            let _slow = crate::perf::SlowJob::new("offline mode toggle");
            offline_mode.set(toggle.is_active());
        }
    });

    // The view-options popover's "Downloaded only" switch is the exact same shared state as
    // `offline_toggle` (ui-spec: "matching Home's offline-mode behavior... scoped to this
    // library") — a second widget over one boolean, not an independent setting.
    let downloaded_only_switch_handler = downloaded_only_switch.connect_state_set({
        let offline_mode = widgets.offline_mode.clone();
        move |_, active| {
            let _slow = crate::perf::SlowJob::new("offline mode toggle");
            offline_mode.set(active);
            glib::signal::Propagation::Proceed
        }
    });

    widgets.offline_mode.add_listener({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let offline_toggle = offline_toggle.clone();
        let downloaded_only_switch = downloaded_only_switch.clone();
        let offline_banner = offline_banner.clone();
        let server_id = offline_toggle_server_id;
        let toast_overlay = toast_overlay.clone();
        move |active| {
            // Sync both widgets' visual state without re-triggering their own handlers. When one
            // of them caused the change, its own `is_active()` already equals `active` (GTK
            // flips it before the handler runs), so this is a no-op there and only actually
            // touches a widget when a *different* trigger (the other switch, or Home's own
            // toggle) changed it.
            if offline_toggle.is_active() != active {
                offline_toggle.block_signal(&offline_toggle_handler);
                offline_toggle.set_active(active);
                offline_toggle.unblock_signal(&offline_toggle_handler);
            }
            if downloaded_only_switch.is_active() != active {
                downloaded_only_switch.block_signal(&downloaded_only_switch_handler);
                downloaded_only_switch.set_active(active);
                downloaded_only_switch.unblock_signal(&downloaded_only_switch_handler);
            }
            offline_banner.set_reveal_child(active);
            update_view_options_indicator(&widgets);
            // Rendered on the next idle (with the busy scrim meanwhile), before any DB work: the
            // visible filter change must not be gated behind the refetch's pool acquire — on a
            // contended pool that await has been observed stalling for tens of seconds. Not
            // inside this handler: GTK only redraws the toggle after the handler returns, and
            // rebuilding the list first made the toggle seem not to react. The in-memory
            // `downloaded` set is whatever the last full load saw, which is immediate-and-slightly-
            // stale; the spawned refetch below re-renders with fresh data when it lands.
            request_render(&widgets);
            glib::spawn_future_local({
                let pool = pool.clone();
                let widgets = widgets.clone();
                let server_id = server_id.clone();
                let toast_overlay = toast_overlay.clone();
                async move {
                    // Refetched here rather than trusting whatever `data.downloaded` last held
                    // from the sync pipeline — a download can complete in the background (the
                    // Player screen's download button) well after Library's last full load, and
                    // toggling offline mode should reflect the *current* download state, not a
                    // stale snapshot.
                    match abs_core::download_tracks::downloaded_item_ids(&pool, &server_id).await {
                        Ok(downloaded) => widgets.data.borrow_mut().downloaded = downloaded.into_iter().collect(),
                        Err(err) => crate::error_reporting::report_background_error(&toast_overlay, &tr("Refreshing downloaded items"), err),
                    }
                    render_from_current_data(&widgets);

                    // `LibraryViewOptions.downloaded_only` is a Library-specific concept — the
                    // shared `OfflineModeState` deliberately knows nothing about it — so this
                    // screen keeps it in sync itself, now from either screen's toggle rather than
                    // only its own (fixing a related bug: this used to never fire from a
                    // Home-driven toggle at all). A single targeted write, not the full
                    // load-mutate-save round trip over all four view-option fields.
                    //
                    // Logged, not toasted: `OfflineModeState` already reports its own write of
                    // this same toggle to the shell, and both writes hit the same settings table,
                    // so one failure would otherwise toast twice.
                    if let Err(err) = abs_core::settings::set_downloaded_only(&pool, active).await {
                        tracing::warn!(%err, "couldn't mirror offline mode into the Library view options");
                    }
                }
            });
        }
    });

    // "Hide finished" — the sheet's own switch, persisted via `spawn_persist_view_options`
    // exactly like the grouping/sort-by combos below.
    hide_finished_switch.connect_state_set({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, active| {
            set_hide_finished(&widgets, active);
            spawn_persist_view_options(pool.clone(), toast_overlay.clone(), widgets.clone());
            glib::signal::Propagation::Proceed
        }
    });

    // "In progress only" — moved here from the now-removed "Sort & filter" popover, and unlike
    // that popover's checkbox, this switch persists (confirmed with the user). `changed` is
    // computed before `set_in_progress_only` writes the Cell so a re-entrant `state-set` (fired
    // when `set_in_progress_only` calls `set_active` back on this same switch — a no-op the
    // second time, since by then `is_active()` already matches) can't persist twice.
    in_progress_only_switch.connect_state_set({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let toast_overlay = toast_overlay.clone();
        move |_, active| {
            let changed = widgets.in_progress_only.get() != active;
            set_in_progress_only(&widgets, active);
            if changed {
                spawn_persist_view_options(pool.clone(), toast_overlay.clone(), widgets.clone());
            }
            glib::signal::Propagation::Proceed
        }
    });

    // Category chips (Author/Series) and the popover's own "Grouping" combo both drive the one
    // persisted `grouping` Cell through `set_grouping` — see this function group's doc comments.
    category_all.connect_toggled({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let toast_overlay = toast_overlay.clone();
        let genre_chip_revealer = genre_chip_revealer.clone();
        move |toggle| {
            if toggle.is_active() {
                genre_chip_revealer.set_reveal_child(false);
                *widgets.category_filter.borrow_mut() = CategoryFilter::All;
                set_grouping(&widgets, Grouping::None);
                spawn_persist_view_options(pool.clone(), toast_overlay.clone(), widgets.clone());
            }
        }
    });
    category_author.connect_toggled({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let toast_overlay = toast_overlay.clone();
        let genre_chip_revealer = genre_chip_revealer.clone();
        move |toggle| {
            if toggle.is_active() {
                genre_chip_revealer.set_reveal_child(false);
                *widgets.category_filter.borrow_mut() = CategoryFilter::Author;
                set_grouping(&widgets, Grouping::ByAuthor);
                spawn_persist_view_options(pool.clone(), toast_overlay.clone(), widgets.clone());
            }
        }
    });
    category_series.connect_toggled({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let toast_overlay = toast_overlay.clone();
        let genre_chip_revealer = genre_chip_revealer.clone();
        move |toggle| {
            if toggle.is_active() {
                genre_chip_revealer.set_reveal_child(false);
                *widgets.category_filter.borrow_mut() = CategoryFilter::Series;
                set_grouping(&widgets, Grouping::BySeries);
                spawn_persist_view_options(pool.clone(), toast_overlay.clone(), widgets.clone());
            }
        }
    });
    // Genre doesn't set `grouping` at all (see `CategoryFilter`'s doc) — just reveals the
    // second row of actual genre values; picking one of *those* is what actually filters (see
    // `rebuild_genre_chips`), and it's session-only (`LibraryViewOptions` has no genre field).
    category_genre.connect_toggled({
        let genre_chip_revealer = genre_chip_revealer.clone();
        move |toggle| genre_chip_revealer.set_reveal_child(toggle.is_active())
    });

    grouping_row.connect_selected_notify({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let toast_overlay = toast_overlay.clone();
        let category_all = category_all.clone();
        let category_author = category_author.clone();
        let category_series = category_series.clone();
        move |row| {
            let grouping = match row.selected() {
                1 => Grouping::BySeries,
                2 => Grouping::ByAuthor,
                _ => Grouping::None,
            };
            // Keep the category chips visually consistent with the combo — activating a grouped
            // toggle button deactivates its siblings automatically (see their own handlers
            // above), and `set_active(true)` on one already active is a no-op, so this can't
            // loop back into re-persisting from here.
            match grouping {
                Grouping::None => category_all.set_active(true),
                Grouping::BySeries => category_series.set_active(true),
                Grouping::ByAuthor => category_author.set_active(true),
            }
            set_grouping(&widgets, grouping);
            spawn_persist_view_options(pool.clone(), toast_overlay.clone(), widgets.clone());
        }
    });

    // The sheet's persisted "Sort by" — the only place sorting is applied (see this module's doc
    // comment).
    sort_by_row.connect_selected_notify({
        let pool = pool.clone();
        let widgets = widgets.clone();
        let toast_overlay = toast_overlay.clone();
        move |row| {
            let sort_by = match row.selected() {
                1 => SortBy::Title,
                2 => SortBy::Author,
                3 => SortBy::Duration,
                4 => SortBy::LastListened,
                _ => SortBy::DateOfCreation,
            };
            widgets.sort.set(SortKey::from_sort_by(sort_by));
            request_render(&widgets);
            spawn_persist_view_options(pool.clone(), toast_overlay.clone(), widgets.clone());
        }
    });

    settings_row.connect_activated({
        let view_options_popover = view_options_popover.clone();
        let on_open_settings = on_open_settings.clone();
        move |_| {
            view_options_popover.popdown();
            on_open_settings();
        }
    });

    // Refreshes every sheet row's *displayed* selection from the live Cells right as the popover
    // opens, rather than trying to keep them permanently in sync with every possible writer (the
    // category chips, Home's shelf tap-through, a future launch's persisted load) — simpler, and
    // the sheet is only ever looked at while it's open.
    view_options_popover.connect_visible_notify({
        let widgets = widgets.clone();
        let downloaded_only_switch = downloaded_only_switch.clone();
        let in_progress_only_switch = in_progress_only_switch.clone();
        let hide_finished_switch = hide_finished_switch.clone();
        let grouping_row = grouping_row.clone();
        let sort_by_row = sort_by_row.clone();
        move |popover| {
            if !popover.is_visible() {
                return;
            }
            downloaded_only_switch.set_active(widgets.offline_mode.get());
            in_progress_only_switch.set_active(widgets.in_progress_only.get());
            hide_finished_switch.set_active(widgets.hide_finished.get());
            grouping_row.set_selected(match widgets.grouping.get() {
                Grouping::None => 0,
                Grouping::BySeries => 1,
                Grouping::ByAuthor => 2,
            });
            sort_by_row.set_selected(match widgets.sort.get().to_sort_by() {
                SortBy::DateOfCreation => 0,
                SortBy::Title => 1,
                SortBy::Author => 2,
                SortBy::Duration => 3,
                SortBy::LastListened => 4,
            });
        }
    });

    // Loaded once, alongside the view mode above — seeds `sort`/`in_progress_only`/
    // `hide_finished`/`grouping` from whatever was last persisted, instead of always starting
    // from the same hardcoded defaults.
    glib::spawn_future_local({
        let widgets = widgets.clone();
        let pool = pool.clone();
        async move {
            if let Ok(options) = abs_core::settings::load_library_view_options(&pool).await {
                widgets.sort.set(SortKey::from_sort_by(options.sort_by));
                widgets.in_progress_only.set(options.in_progress_only);
                widgets.hide_finished.set(options.hide_finished);
                widgets.grouping.set(options.grouping);
                widgets.progress_banner.set_reveal_child(options.in_progress_only);
                widgets.in_progress_only_switch.set_active(options.in_progress_only);
                update_view_options_indicator(&widgets);
                render_from_current_data(&widgets);
            }
        }
    });

    let ctx = SyncCtx {
        pool: pool.clone(),
        paths: paths.clone(),
        session: session.clone(),
        server_id: server.id.clone(),
        account_id: account.id.clone(),
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

    // Home's own syncs ("Sync now" there) land here too. Dropped once this screen is no longer
    // in a window (the shell was rebuilt).
    crate::sync_coordinator::on_synced(&server.id, &account.id, crate::sync_coordinator::Screen::Library, {
        let (pool, server_id, account_id, widgets) = (pool.clone(), server.id.clone(), account.id.clone(), widgets.clone());
        move |outcome| {
            if widgets.scroller.root().is_none() {
                return false;
            }
            glib::spawn_future_local(follow_home_sync(pool.clone(), server_id.clone(), account_id.clone(), widgets.clone(), Some(outcome)));
            true
        }
    });

    // The two manual triggers — the ⋯ menu's "Sync now" and a pull past the scroller's top —
    // share one `ManualSync` (indicator + toast + in-flight guard) between them, mirroring
    // home.rs.
    let manual_sync = crate::widgets::ManualSync::new(&toast_overlay, &pull_indicator);
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
    // The banner's one action is "Retry" or "Log in again", whichever its failure offers; the
    // login one routes to the shell, same as Home's.
    {
        let ctx = ctx.clone();
        let widgets = widgets.clone();
        let manual_sync = manual_sync.clone();
        let on_relogin = on_relogin.clone();
        banner.action_button().connect_clicked(move |_| {
            if widgets.banner.offers_login() {
                on_relogin();
            } else {
                spawn_sync_cycle(ctx.clone(), widgets.clone(), Some(manual_sync.clone()));
            }
        });
    }

    LibraryScreen {
        root: toast_overlay.clone().upcast(),
        search_entry: search_entry.clone(),
        widgets: widgets.clone(),
        pool: pool.clone(),
        server_id: server.id.clone(),
        #[cfg(test)]
        hooks: TestHooks {
            status_page,
            flow_box,
            list_box,
            search_entry,
            banner,
            progress_banner,
            progress_show_all,
            view_toggle,
            offline_toggle,
            offline_banner,
            sync_now_button,
            toast_overlay,
            scroller,
            view_options_button,
            view_options_popover,
            downloaded_only_switch,
            in_progress_only_switch,
            hide_finished_switch,
            grouping_row,
            sort_by_row,
            settings_row,
            category_all,
            category_author,
            category_series,
            category_genre,
            genre_chip_box,
            genre_chip_revealer,
            busy_spinner,
            busy_scrim,
            pull_spinner: pull_indicator.spinner().clone(),
        },
    }
}

/// What a sync cycle needs, bundled for re-spawning — the manual triggers (the ⋯ menu's
/// "Sync now", pull-to-refresh) re-run the exact cycle the screen opens with. Mirrors home.rs's
/// `SyncCtx`; `server`/`account` ride along as the ids the pipeline actually uses.
#[derive(Clone)]
struct SyncCtx {
    pool: SqlitePool,
    paths: AppPaths,
    session: abs_core::auth::Session,
    server_id: String,
    account_id: String,
}

/// Runs one full sync cycle on the main loop — the same 3-phase render shape as `home.rs`:
/// render from cache immediately, sync + reconcile progress, re-render, then fetch cover art
/// concurrently for everything just rendered and re-render once more. This is the only place
/// this screen touches `abs_core`; it never imports `abs_api` at all. Called once when the
/// screen is built and again on every manual trigger, so it must leave all widget state
/// consistent no matter how many times it runs.
///
/// As in home.rs, the network/DB pipeline runs inside `tokio::spawn` (worker threads) — the
/// `spawn_future_local` future is polled on the GTK main thread, and doing HTTP body handling,
/// statement building and cache writes directly there steals frames from the main loop. The
/// main context only parks on the `JoinHandle`s and applies widget updates between stages.
///
/// A manual trigger (`manual`) reports its outcome as a toast and shares an in-flight guard
/// with the screen's other manual trigger — see `crate::widgets::ManualSync`. The automatic
/// cycle passes none and stays unguarded.
fn spawn_sync_cycle(ctx: SyncCtx, widgets: LibraryWidgets, manual: Option<crate::widgets::ManualSync>) {
    if let Some(manual) = &manual {
        if !manual.claim() {
            return;
        }
    }

    // Offline mode: nothing may reach the server — show what's stored locally and stop there.
    if ctx.session.is_offline() {
        tracing::info!("offline mode: Library shows what's stored on the device; not syncing");
        glib::spawn_future_local(render_from_cache(ctx.pool.clone(), ctx.server_id.clone(), ctx.account_id.clone(), widgets.clone()));
        if let Some(manual) = manual {
            manual.finish_offline();
        }
        return;
    }

    // An *automatic* cycle (no `manual`) is what `build()` fires once, immediately — and Home
    // does the exact same thing for the same account, since both screens are built eagerly. Only
    // one of them should actually hit the network; see `sync_coordinator`'s module doc. A manual
    // trigger is user-initiated and always runs for real regardless — nothing else races it.
    let is_automatic = manual.is_none();
    if is_automatic && !crate::sync_coordinator::claim_startup_sync(&ctx.server_id, &ctx.account_id) {
        let (pool, server_id, account_id) = (ctx.pool.clone(), ctx.server_id.clone(), ctx.account_id.clone());
        glib::spawn_future_local(render_from_cache(pool.clone(), server_id.clone(), account_id.clone(), widgets.clone()));
        crate::sync_coordinator::on_completed(&ctx.server_id, &ctx.account_id, move |outcome| {
            glib::spawn_future_local(follow_home_sync(pool, server_id, account_id, widgets, outcome));
        });
        return;
    }

    glib::spawn_future_local(async move {
        let SyncCtx { pool, paths, session, server_id, account_id } = ctx;
        // One log line per load, step by step (`library load finished …`): what a slow library
        // actually spent its time on, and how much of it the screen was frozen.
        let mut steps = crate::perf::Steps::new("library load", if manual.is_some() { "manual" } else { "automatic" });

        steps.step("cached read");
        let cached = load(&pool, &server_id, &account_id).await;
        steps.step("cached render");
        if let Ok(data) = cached {
            apply(data, &widgets);
        }
        steps.close();

        let spawned_sync = tokio::spawn({
            let pool = pool.clone();
            let session = session.clone();
            let sync_paths = paths.clone();
            let server_id = server_id.clone();
            let account_id = account_id.clone();
            async move {
                // Asked at call time, not captured at build time — see home.rs's pipeline note.
                // The connection (settings + resolved base URL) is asked the same way, so a
                // settings change is honored by the next sync cycle.
                let access_token = session.access_token().await;
                let connection = match session.connection_target().await {
                    Ok(connection) => connection,
                    Err(err) => {
                        tracing::warn!(%err, "couldn't load the server's connection settings; sync skipped");
                        return (Err(err), std::time::Duration::ZERO, std::time::Duration::ZERO);
                    }
                };
                let started = std::time::Instant::now();
                let sync_result = abs_core::sync::sync_all(&pool, &connection, &sync_paths, &server_id, &access_token).await;
                let sync_took = started.elapsed();

                let started = std::time::Instant::now();
                if let Err(err) = abs_core::progress_sync::reconcile_all_progress(&pool, &connection, &access_token, &account_id, &server_id, crate::sync_coordinator::loaded_item(&server_id, &account_id).as_deref()).await
                {
                    tracing::warn!(%err, "couldn't reconcile progress with the server; showing local progress");
                }

                (sync_result, sync_took, started.elapsed())
            }
        });
        let (sync_result, sync_took, reconcile_took) = spawned_sync.await.expect("the Library sync task must not panic");
        crate::sync_coordinator::network_used(&server_id, &account_id);
        steps.record("sync", sync_took);
        steps.record("reconcile", reconcile_took);
        // Offline mode switched on mid-sync cut it short; that's not a failure to show.
        let sync_result = sync_result.or_else(|err| if matches!(err, abs_core::CoreError::Offline) { Ok(()) } else { Err(err) });
        let manual_ok = sync_result.is_ok();

        steps.step("reread");
        let data_after_sync = load(&pool, &server_id, &account_id).await;
        let local_read_error = data_after_sync.as_ref().err().map(|err| {
            tracing::warn!(%err, "couldn't read the synced library back from local storage");
            err.to_string()
        });
        let data_after_sync = data_after_sync.ok();
        let item_ids_after_sync: Vec<String> = data_after_sync.as_ref().map(|data| data.items.iter().map(|item| item.id.clone()).collect()).unwrap_or_default();
        let items = data_after_sync.as_ref().map(|data| data.items.len());
        steps.step("rerender");
        if let Some(data) = data_after_sync {
            apply(data, &widgets);
        }
        steps.close();

        show_sync_result(&widgets, sync_result.as_ref().err(), local_read_error.as_deref());

        // Cover art is cosmetic and best-effort (same posture as Home's own cover fetch),
        // fetched concurrently for everything just rendered — after the rest of the screen
        // is already showing (the `apply` above), so a slow/offline server delays only the
        // artwork, never the initial post-sync render. Same two-stage `tokio::spawn` shape
        // as home.rs's identically-named cycle; the batch shares one HTTP client (one pooled
        // connection) across all of it.
        if !item_ids_after_sync.is_empty() {
            // A manual sync also re-asks for covers the server recently said it doesn't have.
            let recheck_missing_covers = manual.is_some();
            steps.step("covers");
            let spawned_covers = tokio::spawn({
                let pool = pool.clone();
                let paths = paths.clone();
                let session = session.clone();
                let server_id = server_id.clone();
                async move {
                    let access_token = session.access_token().await;
                    // Best-effort like the fetches themselves: a settings failure here just
                    // means no covers — logged (above), never surfaced.
                    if let Some(connection) = session.connection_target().await.ok().as_ref() {
                        abs_core::covers::fetch_and_cache_covers(&paths, &pool, connection, &access_token, &server_id, item_ids_after_sync, recheck_missing_covers).await;
                    }
                }
            });
            spawned_covers.await.expect("the Library cover-fetch task must not panic");

            steps.step("final read and render");
            if let Ok(data) = load(&pool, &server_id, &account_id).await {
                apply(data, &widgets);
            }
        }
        steps.finish(if sync_result.is_ok() { "ok" } else { "failed" }, items);

        // The manual trigger's own feedback — after the resolve above, so the banner already
        // shows whatever the toast is about; "Sync failed" carries no details itself.
        if let Some(manual) = manual {
            manual.finish(manual_ok);
        }

        // Only the cycle that actually claimed the automatic sync (see above) reports back —
        // Home, if it lost the claim, is waiting on exactly this to re-render from what just
        // landed.
        if is_automatic {
            crate::sync_coordinator::mark_completed(&server_id, &account_id, crate::sync_coordinator::SyncOutcome::from_result(&sync_result));
        } else {
            crate::sync_coordinator::notify_synced(&server_id, &account_id, crate::sync_coordinator::Screen::Library, crate::sync_coordinator::SyncOutcome::from_result(&sync_result));
        }
    });
}

/// Shows how a sync ended on the banner (Library has no full-page error state: its empty
/// state is "no items", and a failure with nothing saved still shows the banner over it).
/// Called by whichever of Home and Library ran the startup sync and by the one that waited for
/// it (`sync_coordinator::SyncOutcome`), so both tell the same story.
fn show_sync_result(widgets: &LibraryWidgets, error: Option<&CoreError>, local_read_error: Option<&str>) {
    match (error, local_read_error) {
        (Some(err), _) => widgets.banner.show_sync_failure(err),
        // The sync worked but the result couldn't be read back: without this the grid
        // silently keeps showing stale (or no) items, which reads as "nothing here".
        (None, Some(read_error)) => {
            widgets.banner.set_title(&tr("Couldn't read what's saved on this device"));
            widgets.banner.set_description(Some(&tr("The list may be out of date. Try again, or restart the app.")));
            widgets.banner.set_action_label(None);
            widgets.banner.set_details(Some(read_error));
            widgets.banner.set_revealed(true);
        }
        (None, None) => widgets.banner.set_revealed(false),
    }
}

/// Reads whatever's currently cached locally and renders it — never talks to the network. Used
/// both as the ordinary "show cached data immediately, before syncing" step of a real cycle, and
/// as the *entire* response for a screen that lost the automatic-sync claim (see
/// `sync_coordinator`): called once right away and once more when the winning screen's own cycle
/// completes.
async fn render_from_cache(pool: SqlitePool, server_id: String, account_id: String, widgets: LibraryWidgets) {
    if let Ok(data) = load(&pool, &server_id, &account_id).await {
        apply(data, &widgets);
    }
}

/// The single writer behind every "in progress only" surface — the sheet's switch, the view-
/// options button's filter-active highlight, and the in-view banner all change here, from the one
/// `Cell`, so they can't drift apart — and the visible list re-renders, since this is the only
/// place the `Cell` changes. Writing the switch back is loop-safe: `set_active` to the value it
/// already holds doesn't re-emit `state-set` (and the `state-set` handler routes back here,
/// where the second write is a no-op). Persistence is the sheet switch's own handler's job, not
/// this function's — `apply_view`'s Home tap-through and the banner's "Show all" both call this
/// directly, deliberately without persisting (see `in_progress_only`'s field doc).
fn set_in_progress_only(widgets: &LibraryWidgets, active: bool) {
    widgets.in_progress_only.set(active);
    widgets.progress_banner.set_reveal_child(active);
    update_view_options_indicator(widgets);
    if widgets.in_progress_only_switch.is_active() != active {
        widgets.in_progress_only_switch.set_active(active);
    }
    request_render(widgets);
}

/// The single writer behind the view-options popover's "Hide finished" switch — just the Cell,
/// the indicator, and a re-render; persistence is the caller's job (`spawn_persist_view_options`),
/// same split `apply_view_mode`/the view-mode toggle's save already use.
fn set_hide_finished(widgets: &LibraryWidgets, active: bool) {
    widgets.hide_finished.set(active);
    update_view_options_indicator(widgets);
    request_render(widgets);
}

/// The single writer behind both grouping surfaces — the popover's "Grouping" combo and the
/// Author/Series category chips — mirroring `set_in_progress_only`'s "one Cell, every surface
/// funnels through here" shape. Genre selections never call this (see `CategoryFilter`'s doc).
fn set_grouping(widgets: &LibraryWidgets, grouping: Grouping) {
    widgets.grouping.set(grouping);
    request_render(widgets);
}

/// Whether the view-options button should show its "a filter is active" hint (its funnel in the
/// accent colour, the way the player's sleep-timer button shows a timer) — true iff any of the three filters this screen
/// applies (in-progress-only, hide-finished, or downloaded-only/offline-mode) is currently
/// active. Called from every setter that can flip one of those three (`set_in_progress_only`,
/// `set_hide_finished`, the `offline_mode` listener), so the indicator can't fall out of sync
/// with any of them — this is what the old "Sort & filter" button's funnel icon used to do,
/// carried over onto the button that replaces it.
fn update_view_options_indicator(widgets: &LibraryWidgets) {
    let active = widgets.in_progress_only.get() || widgets.hide_finished.get() || widgets.offline_mode.get();
    if active {
        widgets.view_options_button.add_css_class("accent");
    } else {
        widgets.view_options_button.remove_css_class("accent");
    }
    widgets.view_options_button.set_tooltip_text(Some(&if active { tr("Filter active — view options") } else { tr("View options") }));
}

/// Persists the sheet's four lasting fields as one `LibraryViewOptions` row — called after every
/// sheet/chip change meant to survive a restart (not after a genre pick, which is deliberately
/// session-only, nor after `apply_view`'s Home tap-through). Fire-and-forget with toast-on-failure,
/// the same posture the existing view-mode toggle's own save already uses.
fn spawn_persist_view_options(pool: SqlitePool, toast_overlay: adw::ToastOverlay, widgets: LibraryWidgets) {
    glib::spawn_future_local(async move {
        let options = LibraryViewOptions {
            downloaded_only: widgets.offline_mode.get(),
            in_progress_only: widgets.in_progress_only.get(),
            hide_finished: widgets.hide_finished.get(),
            grouping: widgets.grouping.get(),
            sort_by: widgets.sort.get().to_sort_by(),
        };
        if let Err(err) = abs_core::settings::save_library_view_options(&pool, &options).await {
            crate::error_reporting::report_background_error(&toast_overlay, &tr("Saving view options"), err);
        }
    });
}

/// Rebuilds the Genre category chip's second row from whatever genres are actually present
/// across `data.items` right now — called from `apply()` alongside every other full rebuild in
/// this file, so a genre that's no longer in any synced item can't linger as a stale chip.
/// Picking a chip sets the session-only genre filter (see `CategoryFilter`); it does not touch
/// `grouping` or persist anything.
fn rebuild_genre_chips(widgets: &LibraryWidgets) {
    while let Some(child) = widgets.genre_chip_box.first_child() {
        widgets.genre_chip_box.remove(&child);
    }

    let mut genres: Vec<String> = widgets.data.borrow().items.iter().flat_map(|item| item.genres()).collect();
    genres.sort();
    genres.dedup();

    // Each chip joins the *first* chip's group rather than a separate throwaway anchor widget —
    // every chip here is parented into `genre_chip_box` right after creation, so (unlike a
    // never-parented anchor) nothing here can be finalized out from under GTK's group tracking.
    let mut group_anchor: Option<gtk4::ToggleButton> = None;
    for genre in genres {
        let chip = gtk4::ToggleButton::builder().label(&genre).build();
        if let Some(anchor) = &group_anchor {
            chip.set_group(Some(anchor));
        } else {
            group_anchor = Some(chip.clone());
        }
        crate::a11y::radio(&chip);
        chip.connect_toggled({
            let widgets = widgets.clone();
            let genre = genre.clone();
            move |toggle| {
                if toggle.is_active() {
                    *widgets.category_filter.borrow_mut() = CategoryFilter::Genre(genre.clone());
                    request_render(&widgets);
                }
            }
        });
        widgets.genre_chip_box.append(&chip);
    }
}

/// Re-reads local storage after Home ran a sync (the startup one Library didn't claim, or a
/// manual one) and shows how it went — so a failure never goes unmentioned on this tab just
/// because the other one ran the sync.
async fn follow_home_sync(pool: SqlitePool, server_id: String, account_id: String, widgets: LibraryWidgets, outcome: Option<crate::sync_coordinator::SyncOutcome>) {
    let mut steps = crate::perf::Steps::new("library load", "after Home's sync");
    steps.step("read");
    let loaded = load(&pool, &server_id, &account_id).await;
    let local_read_error = loaded.as_ref().err().map(|err| err.to_string());
    let items = loaded.as_ref().ok().map(|data| data.items.len());
    steps.step("render");
    if let Ok(data) = loaded {
        apply(data, &widgets);
    }
    steps.finish(if local_read_error.is_none() { "ok" } else { "failed" }, items);
    if let Some(outcome) = outcome {
        show_sync_result(&widgets, outcome.error(), local_read_error.as_deref());
    }
}

/// Reads whatever's currently cached locally across every synced library — never talks to the
/// network. Unlike Home's `load()`, nothing is truncated or pre-sorted here: filtering/sorting for
/// display happens in `render_visible` against the search text, the filter and the chosen
/// `SortKey`. The account's progress rows ride along in the same pass — they back the
/// `LastListened` sort and the "In progress only" filter.
async fn load(pool: &SqlitePool, server_id: &str, account_id: &str) -> CoreResult<LibraryData> {
    let libraries = abs_storage::repo::libraries::list_for_server(pool, server_id).await?;
    let mut items = Vec::new();
    for library in &libraries {
        items.extend(abs_storage::repo::items::list_for_library(pool, server_id, &library.id).await?);
    }
    let downloaded = abs_core::download_tracks::downloaded_item_ids(pool, server_id).await?.into_iter().collect();
    let last_listened = abs_storage::repo::progress::list_for_account(pool, account_id).await?.into_iter().map(|progress| (progress.item_id.clone(), progress)).collect();
    Ok(LibraryData { items, downloaded, last_listened })
}

fn apply(data: LibraryData, widgets: &LibraryWidgets) {
    *widgets.data.borrow_mut() = data;
    rebuild_genre_chips(widgets);
    render_from_current_data(widgets);
}

/// Shows or hides the busy overlay (dimming scrim + spinner) over the library content — see
/// `request_render`'s doc for why this exists. Both widgets are shown/hidden together; the scrim
/// has no animation of its own to start/stop.
fn set_busy(widgets: &LibraryWidgets, busy: bool) {
    widgets.busy_scrim.set_visible(busy);
    widgets.busy_spinner.set_visible(busy);
    widgets.busy_spinner.set_spinning(busy);
}

/// Loads the `library-busy-scrim`/`library-busy-spinner` CSS classes once per process — same
/// `Once`-guarded `CssProvider` idiom `screens::connection::ensure_mono_css` already uses. The
/// scrim is a plain dark semi-opaque fill (`GtkOverlay` already sizes it to match the main child,
/// per `build()`'s comment, so no width/height rule is needed here); the spinner gets a white
/// foreground on top of it so the spinning dashes read clearly regardless of what's behind the
/// overlay.
fn ensure_busy_overlay_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(
            "box.library-busy-scrim { background-color: rgba(0, 0, 0, 0.45); } \
             spinner.library-busy-spinner { color: white; }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("a display for the app's css"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

/// Shows the busy spinner immediately, then defers the actual rebuild by one main-loop idle
/// tick — same idiom `apply_view_mode` already uses — so GTK gets a chance to paint the spinner
/// before the (multi-second, on a large library) freeze `render_from_current_data` causes. Every
/// filter/search/sort trigger that re-renders goes through this, not `render_from_current_data`
/// directly, with one deliberate exception: the offline/downloaded-only toggle's render (see its
/// own call sites' comments) must stay synchronous.
fn request_render(widgets: &LibraryWidgets) {
    set_busy(widgets, true);
    let widgets = widgets.clone();
    glib::idle_add_local_once(move || {
        render_from_current_data(&widgets);
        set_busy(&widgets, false);
    });
}

/// Applies a view mode to every widget it affects — the toggle button's own icon/tooltip, which
/// container is visible, and a re-render — and updates `widgets.view_mode` first so that
/// re-render sees the new mode. Shared by the toggle's `connect_toggled` handler and by restoring
/// the persisted mode at startup, so both paths stay in sync by construction rather than by
/// keeping two copies of this logic in step by hand.
///
/// The actual container swap + rebuild is deferred by one main-loop idle tick, rather than run
/// synchronously here: on a large library that rebuild (destroying and reconstructing every
/// visible card/row) can take a couple of seconds, and running it inline would freeze the screen
/// before GTK ever gets a chance to paint the icon flip or the busy spinner this function shows
/// first. Deferring it one tick lets that feedback actually reach the screen before the freeze,
/// same `glib::idle_add_local_once` idiom `render_from_current_data` already uses to defer cover
/// decoding until after layout. Doesn't go through `request_render` because it also needs the
/// container swap to happen inside that same deferred tick, before the render.
fn apply_view_mode(mode: LibraryViewMode, widgets: &LibraryWidgets, toggle: &gtk4::ToggleButton) {
    widgets.view_mode.set(mode);
    toggle.set_icon_name(if mode == LibraryViewMode::List { "view-grid-symbolic" } else { "view-list-symbolic" });
    toggle.set_tooltip_text(Some(&if mode == LibraryViewMode::List { tr("Grid view") } else { tr("List view") }));
    set_busy(widgets, true);
    let widgets = widgets.clone();
    glib::idle_add_local_once(move || {
        widgets.flow_box.set_visible(mode == LibraryViewMode::Grid);
        widgets.list_box.set_visible(mode == LibraryViewMode::List);
        // The container no longer shown keeps nothing: its whole tree (a widget per book, with
        // the textures it showed) used to stay alive behind the visible one.
        match mode {
            LibraryViewMode::Grid => clear_list_box(&widgets.list_box),
            LibraryViewMode::List => clear_flow_box(&widgets.flow_box),
        }
        *widgets.rendered_signature.borrow_mut() = None;
        render_from_current_data(&widgets);
        set_busy(&widgets, false);
    });
}

fn render_from_current_data(widgets: &LibraryWidgets) {
    let _slow = crate::perf::SlowJob::new("library render");
    let render_started = std::time::Instant::now();
    let blocked_at_start = crate::perf::main_loop_blocked_total();
    let query = abs_core::search::normalize_for_search(&widgets.search_entry.text());
    let sort = widgets.sort.get();
    let data = widgets.data.borrow();

    let offline_mode = widgets.offline_mode.get();
    let mut visible: Vec<&Item> = data
        .items
        .iter()
        .filter(|item| {
            query.is_empty()
                || abs_core::search::normalize_for_search(&item.title).contains(&query)
                || item.author.as_deref().is_some_and(|author| abs_core::search::normalize_for_search(author).contains(&query))
        })
        .filter(|item| !offline_mode || data.downloaded.contains(&item.id))
        // "In progress only" (Home's Continue Listening tap-through, or the popover's check):
        // needs a progress row that exists *and* isn't finished — a completed book has both,
        // a never-played one has neither.
        .filter(|item| {
            !widgets.in_progress_only.get()
                || data.last_listened.get(&item.id).is_some_and(|progress| !progress.is_finished)
        })
        // "Hide finished" (the view-options sheet's own switch) — the inverse condition from
        // "In progress only" above, and independently settable from it.
        .filter(|item| {
            !widgets.hide_finished.get() || !data.last_listened.get(&item.id).is_some_and(|progress| progress.is_finished)
        })
        // The Genre category chip's own filter, and `OneSeries`'s exact-match — both session-only,
        // see `CategoryFilter`'s doc. The Author/Series chips never reach here: they set
        // `grouping` instead (below), not this.
        .filter(|item| match &*widgets.category_filter.borrow() {
            CategoryFilter::Genre(genre) => item.genres().iter().any(|g| g == genre),
            CategoryFilter::OneSeries(name) => item.series_name.as_deref() == Some(name.as_str()),
            CategoryFilter::All | CategoryFilter::Author | CategoryFilter::Series => true,
        })
        .collect();

    match sort {
        SortKey::DateAdded => visible.sort_by_key(|item| std::cmp::Reverse(item.added_at)),
        SortKey::Title => visible.sort_by_key(|a| a.title.to_lowercase()),
        SortKey::Author => visible.sort_by_cached_key(|item| author_sort_key(item)),
        SortKey::Duration => visible.sort_by(|a, b| b.duration_seconds.total_cmp(&a.duration_seconds)),
        // Played before never-played; within played, newest last-listen first. `updated_at`
        // carries the *true* last-listened time (the server's own `lastUpdate` on import), so
        // this reads as "what was I listening to lately", not "what got imported lately".
        SortKey::LastListened => visible.sort_by(|a, b| match (data.last_listened.get(&a.id), data.last_listened.get(&b.id)) {
            (Some(a_progress), Some(b_progress)) => b_progress.updated_at.cmp(&a_progress.updated_at),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }),
    }

    let has_visible = !visible.is_empty();
    let grouping = widgets.grouping.get();

    // Grouped section headers (ui-spec: "sticky ... when sorted/grouped by author or series" —
    // *grouped*, not sticky, here; see this module's doc comment for why). A stable sort by
    // group key makes same-key items contiguous without disturbing their relative order from
    // the sort above, so each bucket is still internally sorted by `sort`.
    if grouping != Grouping::None {
        visible.sort_by_key(|item| group_key_for(item, grouping).sort_rank());
    }
    let book_count = visible.len();
    let mut groups: Vec<(GroupKey, Vec<&Item>)> = Vec::new();
    for item in visible {
        let key = group_key_for(item, grouping);
        match groups.last_mut() {
            Some((last_key, bucket)) if grouping == Grouping::None || *last_key == key => bucket.push(item),
            _ => groups.push((key, vec![item])),
        }
    }

    // Only the active container is rebuilt — same "full rebuild on every render, not incremental"
    // posture already used everywhere else in this file, just gated per mode so switching modes
    // (or searching/sorting while a mode is hidden) doesn't do wasted work on the other one.
    // Every card/row's cover decode is *deferred* (`build_deferred`/`library_list_row_deferred`):
    // building the widget itself needs no I/O, so that still happens for everything matching
    // the filter, but only covers within (or near) the visible viewport actually start decoding —
    // see `decode_covers_in_viewport`, scheduled once right after this function returns.
    //
    // Building a few hundred cards is seconds of main-loop time on a phone, during which nothing
    // else runs — including every database future. So only the first `FIRST_RENDER_BATCH` entries
    // are built here; the rest follow in idle slices (`append_remaining_entries`), and a newer
    // render abandons an older one still in progress.
    let mut entries: Vec<RenderEntry> = Vec::new();
    for (key, bucket) in &groups {
        if grouping != Grouping::None {
            entries.push(RenderEntry::Header(key.label().to_string()));
        }
        for item in bucket {
            entries.push(RenderEntry::Item { item: Box::new((*item).clone()), downloaded: data.downloaded.contains(&item.id) });
        }
    }
    let mode = widgets.view_mode.get();
    let signature = render_signature(mode, &entries);
    if widgets.rendered_signature.borrow().as_deref() == Some(signature.as_str()) {
        // Exactly what is on screen already (a sync that changed nothing, a toggle with nothing to
        // filter, a search whose results didn't change): rebuilding it would only cost time and
        // lose the scroll position.
        tracing::debug!(books = entries.len(), "library render skipped: nothing changed");
    } else {
        *widgets.rendered_signature.borrow_mut() = None;
        let generation = widgets.render_generation.get() + 1;
        widgets.render_generation.set(generation);
        match mode {
            LibraryViewMode::Grid => clear_flow_box(&widgets.flow_box),
            LibraryViewMode::List => clear_list_box(&widgets.list_box),
        }
        widgets.pending_covers.borrow_mut().clear();
        let entries = Rc::new(entries);
        let built = FIRST_RENDER_BATCH.min(entries.len());
        append_entries(widgets, mode, &entries[..built]);
        let timing = RenderTiming { started: render_started, first: render_started.elapsed(), blocked_at_start };
        if built == entries.len() {
            *widgets.rendered_signature.borrow_mut() = Some(signature);
            log_render_finished(entries.len(), 0, &timing, std::time::Duration::ZERO, std::time::Duration::ZERO);
        } else {
            append_remaining_entries(widgets.clone(), mode, entries, built, generation, signature, timing);
        }
    }

    // The result count, on the search entry's description: spoken while the user is typing
    // there, and the one place a screen reader hears a search's outcome. Also on a render that
    // changed nothing (the count is what matters, not whether the list was rebuilt).
    crate::a11y::describe_if_changed(&widgets.search_entry, &ntr("{count} book", "{count} books", book_count as u32));

    // Deferred past this function returning — the cards above were only just inserted, and
    // `compute_bounds` (inside `decode_covers_in_viewport`) needs a completed layout/allocation
    // pass to report real positions; `idle_add_local_once`'s default priority runs after GTK's
    // own resize/allocate processing, same idiom `widgets::swap_content` already relies on
    // elsewhere in this crate for "let GTK finish its own processing first."
    glib::idle_add_local_once({
        let widgets = widgets.clone();
        move || decode_covers_in_viewport(&widgets)
    });

    // This render owns which of the list and the empty state shows, and what the empty state
    // says — every time, not only in offline mode. Found in the 2026-10-12 pass: with offline mode
    // turned off again after an empty offline view, nothing put the list back, and Library said
    // "No items yet" over 24 stored books until the next sync.
    widgets.status_page.set_visible(!has_visible);
    widgets.scroller.set_visible(has_visible);
    if !has_visible {
        let any_downloaded = data.items.iter().any(|item| data.downloaded.contains(&item.id));
        let (title, description) = if data.items.is_empty() {
            (tr("No items yet"), tr("Check your connection and try again."))
        } else if offline_mode && !any_downloaded {
            (tr("No downloaded items"), tr("Download books to listen to them offline."))
        } else {
            (tr("No matches"), tr("Try a different search or filter."))
        };
        widgets.status_page.set_title(&title);
        widgets.status_page.set_description(Some(&description));
    }
}

/// When a render started and how long its first, synchronous batch took — carried into the idle
/// slices that finish it, for [`log_render_finished`].
struct RenderTiming {
    started: std::time::Instant,
    first: std::time::Duration,
    blocked_at_start: std::time::Duration,
}

/// A render that took at least this long, start to fully drawn, is logged at `info`; quicker ones
/// (most search keystrokes) only at `debug`.
const RENDER_LOG_AFTER: std::time::Duration = std::time::Duration::from_millis(300);

/// One line per finished render: how long until every book was on screen (`total_ms`, which
/// includes other work the main loop did between slices), how much of that was this render's own
/// work (`busy_ms`: the first batch plus every slice), the longest slice, and how long the main
/// loop was frozen meanwhile.
fn log_render_finished(books: usize, slices: u32, timing: &RenderTiming, slices_took: std::time::Duration, longest_slice: std::time::Duration) {
    let total = timing.started.elapsed();
    let blocked = crate::perf::main_loop_blocked_total().saturating_sub(timing.blocked_at_start);
    let line = format!(
        "library render finished books={books} slices={slices} total_ms={} busy_ms={} first_ms={} longest_slice_ms={} main_loop_blocked_ms={}",
        total.as_millis(),
        (timing.first + slices_took).as_millis(),
        timing.first.as_millis(),
        longest_slice.as_millis(),
        blocked.as_millis(),
    );
    if total >= RENDER_LOG_AFTER {
        tracing::info!("{line}");
    } else {
        tracing::debug!("{line}");
    }
}

/// How many cards/rows a render builds before yielding to the main loop, and how many each
/// following idle slice builds. A few dozen widgets is a frame or two even on a phone.
const FIRST_RENDER_BATCH: usize = 40;
const RENDER_SLICE: usize = 40;

/// One thing a render puts on screen, owned so the later slices don't depend on `widgets.data`
/// still holding the same items.
#[derive(Clone)]
enum RenderEntry {
    Header(String),
    Item { item: Box<Item>, downloaded: bool },
}

/// Identifies what a render would show — the view mode and every input the card/row builders and
/// the group headers read — so an identical one can be skipped.
fn render_signature(mode: LibraryViewMode, entries: &[RenderEntry]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (mode == LibraryViewMode::Grid).hash(&mut hasher);
    for entry in entries {
        match entry {
            RenderEntry::Header(label) => ("header", label).hash(&mut hasher),
            RenderEntry::Item { item, downloaded } => ("item", format!("{item:?}"), downloaded).hash(&mut hasher),
        }
    }
    format!("{}:{:x}", entries.len(), hasher.finish())
}

/// Builds and inserts the widgets for `entries` into the active container, and tracks their
/// covers for `decode_covers_in_viewport`.
fn append_entries(widgets: &LibraryWidgets, mode: LibraryViewMode, entries: &[RenderEntry]) {
    let mut pending = widgets.pending_covers.borrow_mut();
    for entry in entries {
        match (mode, entry) {
            (LibraryViewMode::Grid, RenderEntry::Header(label)) => {
                let header = grid_group_header(label);
                widgets.flow_box.insert(&header, -1);
                skip_flow_cell(&header);
            }
            (LibraryViewMode::List, RenderEntry::Header(label)) => {
                widgets.list_box.append(&list_group_header(label));
            }
            (LibraryViewMode::Grid, RenderEntry::Item { item, downloaded }) => {
                let subtitle = item_subtitle(item);
                let built = item_card::build_deferred(TILE_SIZE, item, &subtitle, &widgets.on_open, true, *downloaded, None);
                widgets.flow_box.insert(&built.widget, -1);
                skip_flow_cell(&built.widget);
                pending.push(PendingCover { widget: built.widget, cover: built.cover, path: item.cover_cache_path.as_ref().map(std::path::PathBuf::from) });
            }
            (LibraryViewMode::List, RenderEntry::Item { item, .. }) => {
                let built = library_list_row_deferred(item, &widgets.on_open);
                widgets.list_box.append(&built.row);
                pending.push(PendingCover { widget: built.row.upcast(), cover: built.cover, path: item.cover_cache_path.as_ref().map(std::path::PathBuf::from) });
            }
        }
    }
}

/// `GtkFlowBox` wraps every inserted widget in a focusable cell of its own, which made each grid
/// tile two Tab stops (an unnamed cell, then the card). The card is the one stop; the arrow keys
/// still reach it through the cell.
fn skip_flow_cell(inserted: &impl IsA<gtk4::Widget>) {
    if let Some(cell) = inserted.as_ref().parent() {
        cell.set_focusable(false);
    }
}

/// Builds `entries[from..]` in idle slices. Stops (leaving the partly built list for the render
/// that replaced this one to clear) as soon as `widgets.render_generation` has moved on; on
/// finishing, records `signature` as what is on screen and decodes the covers in view.
#[allow(clippy::too_many_arguments)]
fn append_remaining_entries(
    widgets: LibraryWidgets,
    mode: LibraryViewMode,
    entries: Rc<Vec<RenderEntry>>,
    from: usize,
    generation: u64,
    signature: String,
    timing: RenderTiming,
) {
    let next = Cell::new(from);
    let slices = Cell::new(0_u32);
    let slices_took = Cell::new(std::time::Duration::ZERO);
    let longest_slice = Cell::new(std::time::Duration::ZERO);
    glib::idle_add_local(move || {
        if widgets.render_generation.get() != generation {
            tracing::debug!("a library render was replaced before it finished");
            return glib::ControlFlow::Break;
        }
        let _slow = crate::perf::SlowJob::new("library render slice");
        let slice_started = std::time::Instant::now();
        let start = next.get();
        let end = (start + RENDER_SLICE).min(entries.len());
        append_entries(&widgets, mode, &entries[start..end]);
        next.set(end);
        let took = slice_started.elapsed();
        slices.set(slices.get() + 1);
        slices_took.set(slices_took.get() + took);
        longest_slice.set(longest_slice.get().max(took));
        if end < entries.len() {
            return glib::ControlFlow::Continue;
        }
        *widgets.rendered_signature.borrow_mut() = Some(signature.clone());
        log_render_finished(entries.len(), slices.get(), &timing, slices_took.get(), longest_slice.get());
        let widgets = widgets.clone();
        glib::idle_add_local_once(move || decode_covers_in_viewport(&widgets));
        glib::ControlFlow::Break
    });
}

/// Decodes the cover for every currently-tracked card/row (`widgets.pending_covers`) that's
/// within, or close to, the scrolled viewport — called once, deferred, right after every render
/// (`render_from_current_data`) and again, debounced, whenever `widgets.scroller` is scrolled.
/// `CoverImage::set_path` is itself idempotent (a repeated call with the same path no-ops), so
/// calling it unconditionally for everything in range on every invocation is safe and simple —
/// items outside the range are just left alone, still showing whatever they showed before (their
/// placeholder, the first time). Uses real widget allocations (`compute_bounds`), not an estimated
/// row/column count, so it doesn't need to reproduce `GtkFlowBox`'s own wrapping logic to know
/// what's actually on screen.
fn decode_covers_in_viewport(widgets: &LibraryWidgets) {
    // Roughly one further screen's worth of rows in either direction — covers just outside the
    // visible area start decoding before they're actually scrolled into view, so they're ready
    // (or already in flight) by the time they are.
    const MARGIN_PX: f32 = 600.0;

    let viewport_height = widgets.scroller.height() as f32;
    let scroller = widgets.scroller.clone().upcast::<gtk4::Widget>();
    for pending in widgets.pending_covers.borrow().iter() {
        let Some(bounds) = pending.widget.compute_bounds(&scroller) else { continue };
        let top = bounds.y();
        let bottom = top + bounds.height();
        if bottom > -MARGIN_PX && top < viewport_height + MARGIN_PX {
            pending.cover.set_path(pending.path.as_deref());
        }
    }
}

fn item_subtitle(item: &Item) -> String {
    let author = named(&item.author).map_or_else(|| tr("Unknown author"), str::to_string);
    let duration = crate::screens::item_detail::format_duration(item.duration_seconds);
    // TRANSLATORS: A book's subtitle on a card or in a list. {author} is the author's name; {duration} its length, e.g. "11.5h" or "45m".
    tr_args("{author} · {duration}", &[("author", &author), ("duration", &duration)])
}

/// The section an item falls into under a given `Grouping`. `Named` carries a real author/series
/// name; `Fallback` is the catch-all bucket ("Unknown author"/"Other") for items missing that
/// metadata. Kept as a distinct variant (rather than folding the fallback text into `Named`) so
/// [`GroupKey::sort_rank`] can always place it last, however its label happens to compare
/// alphabetically — see that method's doc for why.
#[derive(Clone, PartialEq, Eq)]
enum GroupKey {
    Named(String),
    Fallback(String),
}

impl GroupKey {
    fn label(&self) -> &str {
        match self {
            GroupKey::Named(s) | GroupKey::Fallback(s) => s,
        }
    }

    /// Sorts every named group alphabetically (case-insensitive) ahead of the fallback bucket,
    /// which always sorts last regardless of its label. Without this, a plain alphabetical sort
    /// over the label text alone lets "Other"/"Unknown author" land in an arbitrary slot among
    /// real names — and since most items in a typical library lack series metadata, that one
    /// bucket is usually the largest, so it dominates the list and grouping looks like it did
    /// nothing (the reported bug).
    fn sort_rank(&self) -> (u8, String) {
        match self {
            GroupKey::Named(s) => (0, s.to_lowercase()),
            GroupKey::Fallback(s) => (1, s.to_lowercase()),
        }
    }
}

/// `Grouping::None` is never actually consulted (every item lands in the render loop's single
/// un-headered bucket regardless of what this returns), so its `Named(String::new())` here is
/// just a harmless placeholder.
fn group_key_for(item: &Item, grouping: Grouping) -> GroupKey {
    match grouping {
        Grouping::None => GroupKey::Named(String::new()),
        Grouping::ByAuthor => named(&item.author)
            .map(|author| GroupKey::Named(author.to_string()))
            .unwrap_or_else(|| GroupKey::Fallback(tr("Unknown author"))),
        Grouping::BySeries => named(&item.series_name)
            .map(|series| GroupKey::Named(series.to_string()))
            .unwrap_or_else(|| GroupKey::Fallback(tr_ctx("Group header for books without a series", "Other"))),
    }
}

/// Alphabetical by author, case-insensitively, with books without one after every named one
/// rather than ahead of "A".
fn author_sort_key(item: &Item) -> (bool, String) {
    match named(&item.author) {
        Some(author) => (false, author.to_lowercase()),
        None => (true, String::new()),
    }
}

/// An author or series name, or `None` when there isn't one. Audiobookshelf sends `""` for a book
/// without either; the sync now stores that as `None`, but books synced before still hold `""`
/// until their next sync, and they belong at the bottom with the rest, not first under a blank
/// header.
fn named(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.trim().is_empty())
}

/// A grouped (not sticky — see this module's doc comment) section header for grid mode. Returned
/// as a plain `GtkLabel`, not a `GtkFlowBoxChild` — `FlowBox::insert` already wraps whatever
/// widget it's given in its own `FlowBoxChild` (the same way every item card here is inserted),
/// so wrapping it again here would nest two `FlowBoxChild`s per header.
///
/// Deliberately does **not** try to force itself onto its own full-width line via `hexpand` +
/// an oversized `width_request` — an earlier version did exactly that, and it overflowed the
/// whole window on a real (narrow-phone) device: `flow_box` is `.homogeneous(true)`, which
/// resizes *every* child to the natural width of the single widest one, so a header wider than a
/// tile balloons every tile too, and `flow_box`'s own natural-width request balloons with it;
/// with the outer `scroller`'s `hscrollbar_policy(Never)`, there's no scrollbar to absorb that,
/// so it propagates straight into the window's size. This is the exact same failure mode
/// `widgets::item_card`'s `title_label` doc comment already documents once for a different
/// child in this same box — `max_width_chars` is what actually prevents it, not a width fight.
fn grid_group_header(title: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["heading"])
        .max_width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .can_focus(false)
        .build()
}

/// The list-mode equivalent of [`grid_group_header`] — a plain, non-activatable/non-selectable
/// full-width row, which `GtkListBox` hosts natively (unlike `GtkFlowBox`, no homogeneous-sizing
/// trick needed). Still needs the same `max_width_chars`/`ellipsize` cap `grid_group_header` uses,
/// though: `list_box` isn't in its own scroller — it shares the outer `scroller`'s
/// `hscrollbar_policy(Never)` with `flow_box` via `scroll_content` — so an unclamped label's
/// natural width bubbles straight up into the window's own size the same way, just through
/// `ListBoxRow` instead of a homogeneous `FlowBox` cell. Missed once already (a real-device
/// report after grouping by series in list mode); don't drop it a second time.
fn list_group_header(title: &str) -> gtk4::ListBoxRow {
    let label = gtk4::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["heading"])
        .max_width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .margin_start(4)
        .margin_top(8)
        .margin_bottom(4)
        .build();
    gtk4::ListBoxRow::builder().child(&label).activatable(false).selectable(false).build()
}

/// A list-mode row — same information as a grid tile (cover thumbnail, title, subtitle), just laid
/// out horizontally per `docs/design/ui-spec.md`'s "useful for podcast episode-style feeds" framing.
/// Mirrors `home.rs`'s `library_row()` shape (an `AdwActionRow` with a prefix), swapping the
/// symbolic icon for a small cover thumbnail via the same `CoverImage` widget `item_card.rs` uses.
/// The cover is left on its placeholder — `render_from_current_data`'s viewport-aware lazy decode
/// (see [`PendingCover`]/`decode_covers_in_viewport`) decides when to actually decode it.
struct BuiltListRow {
    row: adw::ActionRow,
    cover: crate::widgets::cover_image::CoverImage,
}

fn library_list_row_deferred(item: &Item, on_open: &Rc<dyn Fn(PlayRequest)>) -> BuiltListRow {
    const THUMBNAIL_SIZE: i32 = 48;

    let cover = crate::widgets::cover_image::CoverImage::new(THUMBNAIL_SIZE);

    let row = adw::ActionRow::builder().title(&item.title).subtitle(item_subtitle(item)).activatable(true).build();
    row.add_prefix(cover.widget());

    let request = PlayRequest { item_id: item.id.clone(), title: item.title.clone(), author: item.author.clone() };
    let on_open = on_open.clone();
    row.connect_activated(move |_| on_open(request.clone()));

    BuiltListRow { row, cover }
}

fn clear_flow_box(fb: &gtk4::FlowBox) {
    while let Some(child) = fb.child_at_index(0) {
        fb.remove(&child);
    }
}

fn clear_list_box(lb: &gtk4::ListBox) {
    while let Some(child) = lb.row_at_index(0) {
        lb.remove(&child);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::{pool, pump_until};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn stored_item(author: Option<&str>, series_name: Option<&str>) -> Item {
        Item {
            id: "item-1".to_string(),
            server_id: "server-1".to_string(),
            library_id: "lib-1".to_string(),
            title: "Book".to_string(),
            author: author.map(str::to_string),
            narrator: None,
            description: None,
            cover_cache_path: None,
            duration_seconds: 3600.0,
            added_at: chrono::Utc::now(),
            synced_at: chrono::Utc::now(),
            series_name: series_name.map(str::to_string),
            genres_json: "[]".to_string(),
            title_sort: None,
            author_sort: None,
        }
    }

    /// Books synced before blank names were dropped at the API still hold `""`.
    #[test]
    fn a_blank_author_or_series_groups_with_the_missing_ones() {
        for blank in [None, Some(""), Some("  ")] {
            let item = stored_item(blank, blank);
            assert!(matches!(group_key_for(&item, Grouping::ByAuthor), GroupKey::Fallback(_)), "{blank:?}");
            assert!(matches!(group_key_for(&item, Grouping::BySeries), GroupKey::Fallback(_)), "{blank:?}");
        }
        let named = stored_item(Some("Andy Weir"), Some("Bobiverse"));
        assert!(group_key_for(&named, Grouping::ByAuthor) == GroupKey::Named("Andy Weir".to_string()));
        assert!(group_key_for(&named, Grouping::BySeries) == GroupKey::Named("Bobiverse".to_string()));
    }

    #[test]
    fn sorting_by_author_puts_books_without_one_last() {
        let books = [stored_item(Some(""), None), stored_item(Some("zola"), None), stored_item(None, None), stored_item(Some("Asimov"), None)];
        let mut sorted: Vec<&Item> = books.iter().collect();
        sorted.sort_by_cached_key(|item| author_sort_key(item));
        let authors: Vec<Option<&str>> = sorted.iter().map(|item| item.author.as_deref()).collect();
        assert_eq!(authors, [Some("Asimov"), Some("zola"), Some(""), None]);
    }

    /// Seeds the library/items/progress rows a scenario's progress-dependent assertions read,
    /// before `build`'s first cached load — the progress table's foreign key needs the items to
    /// exist first. Every write is an idempotent upsert (the sync re-runs the same ones against
    /// the mock server's identical ids), and the library id matches the mock's.
    async fn seed_item_with_progress(pool: &SqlitePool, server_id: &str, account_id: &str, item_id: &str, title: &str, added_at_ms: i64, progress: Option<(f64, bool, chrono::DateTime<chrono::Utc>)>) {
        abs_storage::repo::libraries::upsert(
            pool,
            abs_storage::repo::libraries::UpsertLibrary { id: "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        abs_storage::repo::items::upsert(
            pool,
            abs_storage::repo::items::UpsertItem { id: item_id, server_id, library_id: "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", title, author: None, narrator: None, description: None, duration_seconds: 3600.0, added_at: chrono::DateTime::from_timestamp_millis(added_at_ms).unwrap(), series_name: None, genres: &[] },
        )
        .await
        .unwrap();
        if let Some((position, is_finished, updated_at)) = progress {
            abs_storage::repo::progress::set_at(pool, account_id, server_id, item_id, position, is_finished, updated_at).await.unwrap();
        }
    }

    async fn account_and_server(pool: &SqlitePool, server_url: &str) -> (Server, Account) {
        let server_id = abs_storage::repo::servers::add(pool, server_url).await.unwrap();
        let account_id = abs_storage::repo::accounts::add(pool, &server_id, "jane", "token123", None).await.unwrap();
        (
            abs_storage::repo::servers::get(pool, &server_id).await.unwrap(),
            abs_storage::repo::accounts::get(pool, &account_id).await.unwrap(),
        )
    }

    fn item_json(id: &str, title: &str, author: &str, added_at_ms: i64, duration: f64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "addedAt": added_at_ms,
            "media": {
                "duration": duration,
                "metadata": { "title": title, "authorName": author }
            }
        })
    }

    fn item_json_with_series_and_genres(id: &str, title: &str, series_name: Option<&str>, genres: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "addedAt": 1_700_000_000_000i64,
            "media": {
                "duration": 3600.0,
                "metadata": { "title": title, "seriesName": series_name, "genres": genres }
            }
        })
    }

    pub(crate) fn flow_box_titles(flow_box: &gtk4::FlowBox) -> Vec<String> {
        let mut titles = Vec::new();
        let mut index = 0;
        while let Some(child) = flow_box.child_at_index(index) {
            let button = child.child().and_then(|w| w.downcast::<gtk4::Button>().ok()).expect("flow box child wraps a button");
            let card_box = button.child().and_then(|w| w.downcast::<gtk4::Box>().ok()).expect("button wraps the card box");
            let title_label = card_box
                .first_child()
                .and_then(|cover| cover.next_sibling())
                .and_then(|w| w.downcast::<gtk4::Label>().ok())
                .expect("card's second child is the title label");
            titles.push(title_label.text().to_string());
            index += 1;
        }
        titles
    }

    pub(crate) fn list_box_titles(list_box: &gtk4::ListBox) -> Vec<String> {
        let mut titles = Vec::new();
        let mut index = 0;
        while let Some(row) = list_box.row_at_index(index) {
            let action_row = row.downcast::<adw::ActionRow>().expect("list box row is an AdwActionRow");
            titles.push(action_row.title().to_string());
            index += 1;
        }
        titles
    }

    /// Like `flow_box_titles`, but also reports grouped section headers (`grid_group_header`) —
    /// each one prefixed with `§` so a test can assert both headers and card order in one list
    /// without confusing a header's text for an item title.
    fn flow_box_entries(flow_box: &gtk4::FlowBox) -> Vec<String> {
        let mut entries = Vec::new();
        let mut index = 0;
        while let Some(child) = flow_box.child_at_index(index) {
            let inner = child.child().expect("flow box child has content");
            if let Ok(button) = inner.clone().downcast::<gtk4::Button>() {
                let card_box = button.child().and_then(|w| w.downcast::<gtk4::Box>().ok()).expect("button wraps the card box");
                let title_label = card_box.first_child().and_then(|cover| cover.next_sibling()).and_then(|w| w.downcast::<gtk4::Label>().ok()).expect("card's second child is the title label");
                entries.push(title_label.text().to_string());
            } else if let Ok(label) = inner.downcast::<gtk4::Label>() {
                entries.push(format!("§{}", label.text()));
            }
            index += 1;
        }
        entries
    }

    /// Like `list_box_titles`, but also reports grouped section headers (`list_group_header`) —
    /// see `flow_box_entries`'s doc comment for the `§` prefix convention.
    fn list_box_entries(list_box: &gtk4::ListBox) -> Vec<String> {
        let mut entries = Vec::new();
        let mut index = 0;
        while let Some(row) = list_box.row_at_index(index) {
            if let Ok(action_row) = row.clone().downcast::<adw::ActionRow>() {
                entries.push(action_row.title().to_string());
            } else if let Some(label) = row.child().and_then(|w| w.downcast::<gtk4::Label>().ok()) {
                entries.push(format!("§{}", label.text()));
            }
            index += 1;
        }
        entries
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point.
    /// Collects everything logged while it's the default subscriber, for asserting on log lines.
    #[derive(Clone, Default)]
    struct LogBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl LogBuffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A library load logs one line naming
    /// every step it took (cached read, render, sync, reconcile, re-read, re-render, covers), and a
    /// render built in slices logs one line once every book is on screen — what the phone's log
    /// needs to show where a slow library spends its time.
    pub(crate) fn run_a_library_load_logs_each_step(runtime: &tokio::runtime::Runtime) {
        let log = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let log = log.clone();
                move || log.clone()
            })
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);

        let items: Vec<_> = (0..120)
            .map(|i| item_json(&format!("item-{i}"), &format!("Book {i}"), "Author", 1_700_000_000_000 - i * 1000, 3600.0))
            .collect();
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
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": items })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let _screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, || {});

        pump_until(|| log.text().contains("library load finished"), Duration::from_secs(20));
        pump_until(|| log.text().contains("library render finished books=120"), Duration::from_secs(10));
        let text = log.text();
        let load = text.lines().find(|line| line.contains("library load finished")).unwrap_or_else(|| panic!("no load line in:\n{text}"));
        for field in [
            "trigger=automatic",
            "items=120",
            "total_ms=",
            "cached_read_ms=",
            "cached_render_ms=",
            "sync_ms=",
            "reconcile_ms=",
            "reread_ms=",
            "rerender_ms=",
            "covers_ms=",
            "final_read_and_render_ms=",
            "main_loop_blocked_ms=",
            "outcome=ok",
        ] {
            assert!(load.contains(field), "{field} missing from: {load}");
        }
        let render = text.lines().find(|line| line.contains("library render finished books=120")).unwrap();
        for field in ["slices=2", "total_ms=", "busy_ms=", "first_ms=", "longest_slice_ms=", "main_loop_blocked_ms="] {
            assert!(render.contains(field), "{field} missing from: {render}");
        }
    }

    pub(crate) fn run_renders_all_synced_items(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [
                        { "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" },
                        { "id": "b1a2c3d4-5e6f-4789-a0b1-c2d3e4f56789", "name": "Kids", "mediaType": "book" }
                    ]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "The Martian", "Andy Weir", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/b1a2c3d4-5e6f-4789-a0b1-c2d3e4f56789/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [item_json("item-3", "The Little Red Tractor", "Gus Tanaka", 1_650_000_000_000, 1800.0)]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(0).is_some(), Duration::from_secs(10));

        assert!(!hooks.status_page.is_visible(), "should have left the empty state once items synced");
        let mut index = 0;
        while hooks.flow_box.child_at_index(index).is_some() {
            index += 1;
        }
        assert_eq!(index, 3, "all three items across both libraries should render");
    }

    pub(crate) fn run_search_filters_by_title_and_author(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Dune", "Frank Herbert", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));

        hooks.search_entry.set_text("weir");
        pump_until(|| flow_box_titles(&hooks.flow_box).len() == 1, Duration::from_secs(5));

        let titles = flow_box_titles(&hooks.flow_box);
        assert_eq!(titles, vec!["Project Hail Mary"], "searching an author substring should filter to matching items");

        hooks.search_entry.set_text("dune");
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Dune".to_string()], Duration::from_secs(5));
    }

    /// A plain-ASCII query must still find a title with national characters, and vice versa —
    /// the fix for the reported "laka"/"ląka" should both match "łąka" gap.
    pub(crate) fn run_search_ignores_national_characters(runtime: &tokio::runtime::Runtime) {
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
                    "results": [item_json("item-1", "Łąka", "Autor Polski", 1_700_000_000_000, 3600.0)]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(0).is_some(), Duration::from_secs(10));

        hooks.search_entry.set_text("laka");
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Łąka".to_string()], Duration::from_secs(5));

        hooks.search_entry.set_text("ląka");
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Łąka".to_string()], Duration::from_secs(5));

        hooks.search_entry.set_text("łąka");
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Łąka".to_string()], Duration::from_secs(5));
    }

    /// Toggling offline mode should narrow the grid to items with at least one completed
    /// download, show the banner, and restore the full list when toggled off again.
    pub(crate) fn run_offline_mode_toggle_filters_to_downloaded_items(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Dune", "Frank Herbert", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();
        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));

        // Mark "item-1" as downloaded (one complete track) directly in storage — this test is
        // about the toggle/filter, not the download pipeline itself.
        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-1", &[abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 3600.0, offset_seconds: 0.0, size_bytes: None }])).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::upsert_pending(&pool, &server.id, "item-1", "1", "/p/1.mp3")).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::mark_complete(&pool, &server.id, "item-1", "1", 10)).unwrap();

        // The filter must land synchronously on the toggle, not after the handler's background
        // refetch resolves — that refetch's pool acquire has been observed stalling for tens of
        // seconds on a contended device pool, leaving the grid unfiltered the whole time (and
        // only a view-mode round-trip forced an immediate re-render). The first toggle-on still
        // waits for the refetch here, because the download above was inserted after this
        // screen's load — the in-memory set is stale and the synchronous render legitimately
        // shows nothing — but every later toggle below needs only the next idle: the list
        // re-renders from the by-then-warm in-memory set.
        hooks.offline_toggle.set_active(true);
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Project Hail Mary".to_string()], Duration::from_secs(5));
        assert!(hooks.offline_banner.reveals_child(), "the offline banner should show while the toggle is active");

        // The toggle handler itself only flips cheap state, so the button redraws at once; the
        // list follows on the very next idle, from the in-memory set (no database wait).
        hooks.offline_toggle.set_active(false);
        assert!(!hooks.offline_banner.reveals_child(), "the banner should hide once offline mode is off");
        assert_eq!(flow_box_titles(&hooks.flow_box).len(), 1, "the list isn't rebuilt inside the toggle handler");
        crate::test_support::run_pending_main_loop_work();
        assert_eq!(flow_box_titles(&hooks.flow_box).len(), 2, "toggling offline mode off re-renders on the next idle");

        hooks.offline_toggle.set_active(true);
        crate::test_support::run_pending_main_loop_work();
        assert_eq!(flow_box_titles(&hooks.flow_box), vec!["Project Hail Mary".to_string()], "toggling offline mode on filters on the next idle from the in-memory downloaded set");
    }

    /// Found in the 2026-10-12 pass: offline mode on with nothing to show, then off again, left
    /// the empty state up over a full list until the next sync. And a search with no matches
    /// says so instead of showing a blank page.
    pub(crate) fn run_empty_states_follow_offline_mode_and_search(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Dune", "Frank Herbert", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();
        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));

        // A search that matches nothing.
        hooks.search_entry.set_text("Meditations");
        pump_until(|| hooks.status_page.is_visible(), Duration::from_secs(5));
        assert_eq!(hooks.status_page.title(), "No matches");
        assert!(!hooks.scroller.is_visible());

        // Offline with nothing downloaded, then back online: the list comes back.
        hooks.offline_toggle.set_active(true);
        pump_until(|| hooks.status_page.title() == "No downloaded items", Duration::from_secs(5));
        hooks.offline_toggle.set_active(false);
        crate::test_support::run_pending_main_loop_work();
        hooks.search_entry.set_text("");
        pump_until(|| hooks.scroller.is_visible(), Duration::from_secs(5));
        assert!(!hooks.status_page.is_visible(), "the list is back, not the empty state");
        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(5));
    }

    pub(crate) fn run_sort_changes_order(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Zed Book", "Author A", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Alpha Book", "Author B", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));

        assert_eq!(flow_box_titles(&hooks.flow_box), vec!["Zed Book", "Alpha Book"], "default sort is date-added descending");

        hooks.sort_by_row.set_selected(1); // Title
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Alpha Book".to_string(), "Zed Book".to_string()], Duration::from_secs(5));

        hooks.sort_by_row.set_selected(0); // Date of creation
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Zed Book".to_string(), "Alpha Book".to_string()], Duration::from_secs(5));
    }

    /// "Last listened" orders by actual listening recency — played items newest-listen first,
    /// never-played items after them in stable order — independent of when items were *added*
    /// to the server.
    pub(crate) fn run_sorts_by_last_listened(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Zed Book", "Author A", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Alpha Book", "Author B", 1_600_000_000_000, 3600.0),
                        item_json("item-3", "Middle Book", "Author C", 1_500_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let now = chrono::Utc::now();
        // Added newest-first (the default sort's order), but listened to in the opposite order —
        // plus one item never played at all.
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-1", "Zed Book", 1_700_000_000_000, Some((600.0, false, now - chrono::Duration::days(30)))));
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-2", "Alpha Book", 1_600_000_000_000, Some((600.0, false, now - chrono::Duration::days(1)))));
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-3", "Middle Book", 1_500_000_000_000, None));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(2).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(2).is_some(), Duration::from_secs(10));
        assert_eq!(flow_box_titles(&hooks.flow_box), vec!["Zed Book", "Alpha Book", "Middle Book"], "default sort is date-added descending");

        hooks.sort_by_row.set_selected(4); // Last listened
        pump_until(
            || flow_box_titles(&hooks.flow_box) == vec!["Alpha Book".to_string(), "Zed Book".to_string(), "Middle Book".to_string()],
            Duration::from_secs(5),
        );
    }

    /// The "In progress only" filter, driven through both of its manual entry points (the
    /// sheet's switch and the banner's "Show all") plus the navigation path (`apply_view`, what
    /// Home's Continue Listening header triggers): every surface — banner, funnel icon, the
    /// switch itself — must reflect the one shared state, whichever of them changed it.
    pub(crate) fn run_in_progress_filter_is_manually_toggleable(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Reading Now", "Author A", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Finished Book", "Author B", 1_600_000_000_000, 3600.0),
                        item_json("item-3", "Untouched Book", "Author C", 1_500_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let now = chrono::Utc::now();
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-1", "Reading Now", 1_700_000_000_000, Some((600.0, false, now))));
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-2", "Finished Book", 1_600_000_000_000, Some((3600.0, true, now - chrono::Duration::days(2)))));
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-3", "Untouched Book", 1_500_000_000_000, None));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(2).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(2).is_some(), Duration::from_secs(10));
        assert_eq!(flow_box_titles(&hooks.flow_box).len(), 3, "no filter is active initially");
        assert!(!hooks.progress_banner.reveals_child(), "the filter banner stays hidden while no filter is active");
        assert!(!hooks.in_progress_only_switch.is_active());

        // Via the sheet's switch — the manual path. The banner reveals synchronously (inside
        // `set_in_progress_only`, before its now-deferred re-render), so pumping on it alone
        // would return before the filtered rebuild actually lands — wait on the rebuild itself.
        hooks.in_progress_only_switch.set_active(true);
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Reading Now".to_string()], Duration::from_secs(5));
        assert!(hooks.progress_banner.reveals_child());
        assert!(hooks.in_progress_only_switch.is_active());
        assert_eq!(hooks.view_options_button.icon_name().as_deref(), Some("funnel-symbolic"), "the view-options button always shows the funnel");
        assert!(hooks.view_options_button.has_css_class("accent"), "and highlights it while a filter is on");

        // Via the banner's "Show all" — the escape hatch.
        hooks.progress_show_all.emit_clicked();
        pump_until(|| hooks.flow_box.child_at_index(2).is_some(), Duration::from_secs(5));
        assert!(!hooks.progress_banner.reveals_child());
        assert!(!hooks.in_progress_only_switch.is_active(), "the switch reflects the shared state, not just the banner");
        assert_eq!(hooks.view_options_button.icon_name().as_deref(), Some("funnel-symbolic"), "still the funnel with no filter on");
        assert!(!hooks.view_options_button.has_css_class("accent"), "no highlight with no filter on");

        // Via navigation (`apply_view`) — the Continue Listening header's path. The externally-
        // set state must sync the switch back the other way: switch → state, state → switch.
        screen.apply_view(SortKey::LastListened, true);
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Reading Now".to_string()], Duration::from_secs(5));
        assert!(hooks.in_progress_only_switch.is_active(), "apply_view must sync the sheet's switch to the externally-set state");
        assert!(hooks.progress_banner.reveals_child());
    }

    pub(crate) fn run_shows_a_banner_when_sync_fails(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.banner.widget().reveals_child(), Duration::from_secs(10));

        assert!(hooks.banner.widget().reveals_child(), "a sync failure should show the banner");
        assert!(hooks.status_page.is_visible(), "with nothing cached yet, the empty state stays up too");
    }

    /// Dead session with cached data on the Library tab: the banner must swap its copy and offer
    /// "Log in again" — a retry can't fix a revoked session, and the Library tab is where a user
    /// browsing while the token dies will actually be.
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
        runtime.block_on(abs_storage::repo::libraries::upsert(
            &pool,
            abs_storage::repo::libraries::UpsertLibrary {
                id: "lib-1",
                server_id: &server.id,
                name: "Audiobooks",
                media_type: "book",
                icon: None,
                display_order: 1,
            },
        ))
        .unwrap();

        let relogin_requested = Rc::new(std::cell::Cell::new(false));
        let on_relogin = {
            let relogin_requested = relogin_requested.clone();
            move || relogin_requested.set(true)
        };
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, on_relogin, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.banner.widget().reveals_child(), Duration::from_secs(10));

        assert_eq!(hooks.banner.title(), "Signed out by the server");
        assert!(hooks.banner.action_visible(), "an auth failure must offer Log in again in the banner");
        assert!(hooks.status_page.is_visible(), "no synced items yet, so the status page stays up");
        assert!(!relogin_requested.get(), "merely showing the banner must not trigger a re-login");

        hooks.banner.action_button().emit_clicked();
        assert!(relogin_requested.get(), "the banner's login action must fire on_relogin");
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
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| false, Duration::from_millis(500));

        assert!(hooks.status_page.is_visible(), "no libraries at all should show the empty state");
    }

    pub(crate) fn run_tapping_a_card_invokes_on_open(runtime: &tokio::runtime::Runtime) {
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
                    "results": [item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0)]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let opened: Rc<std::cell::RefCell<Vec<PlayRequest>>> = Rc::new(std::cell::RefCell::new(Vec::new()));
        let on_open = {
            let opened = opened.clone();
            move |request: PlayRequest| opened.borrow_mut().push(request)
        };

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), on_open, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(0).is_some(), Duration::from_secs(10));

        let child = hooks.flow_box.child_at_index(0).unwrap();
        let button = child.child().and_then(|w| w.downcast::<gtk4::Button>().ok()).expect("flow box child wraps a button");
        button.emit_clicked();

        assert_eq!(opened.borrow().len(), 1, "clicking the card should invoke on_open exactly once (opening Item Detail)");
        assert_eq!(opened.borrow()[0].item_id, "item-1");
    }

    pub(crate) fn run_list_view_toggle_switches_visible_container(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "The Martian", "Andy Weir", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        assert!(hooks.list_box.is_visible(), "list should be visible by default");
        assert!(!hooks.flow_box.is_visible());

        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(5));

        assert!(!hooks.list_box.is_visible(), "switching to grid mode should hide the list");
        assert!(hooks.flow_box.is_visible());
        assert_eq!(flow_box_titles(&hooks.flow_box).len(), 2);

        hooks.view_toggle.set_active(true);
        // `list_box` still holds its stale rows from the default render above (only the active
        // container is rebuilt on a switch), so waiting on row presence alone could return
        // immediately without the deferred re-render (and its visibility flip) ever running —
        // wait on visibility instead, which only the deferred callback can flip.
        pump_until(|| hooks.list_box.is_visible(), Duration::from_secs(5));
        assert!(!hooks.flow_box.is_visible());
    }

    pub(crate) fn run_switching_view_mode_shows_a_spinner_immediately(runtime: &tokio::runtime::Runtime) {
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
                    "results": [item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0)]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));
        assert!(!hooks.busy_spinner.is_visible());
        assert!(!hooks.busy_scrim.is_visible(), "the dimming scrim behind the spinner must hide with it");

        hooks.view_toggle.set_active(false);
        // Before pumping the main loop at all: the spinner must already be showing and the
        // rebuild must not have happened yet — proving the feedback lands on the same frame as
        // the tap, ahead of the (deferred) rebuild, not after it.
        assert!(hooks.busy_spinner.is_visible(), "the spinner should appear before the rebuild runs");
        assert!(hooks.busy_scrim.is_visible(), "the dimming scrim should appear together with the spinner");
        assert!(hooks.list_box.is_visible(), "the old mode's container should still be showing");

        pump_until(|| hooks.flow_box.child_at_index(0).is_some(), Duration::from_secs(5));
        assert!(!hooks.busy_spinner.is_visible(), "the spinner should hide once the rebuild is done");
        assert!(!hooks.busy_scrim.is_visible(), "the scrim should hide with it");
        assert!(hooks.flow_box.is_visible());
    }

    /// Regression test: the busy spinner used to be wired only to the view-mode toggle above —
    /// every other filter/search trigger (category chips, search, the sheet's switches/combos)
    /// called the same expensive rebuild directly, with no feedback, which is exactly the freeze
    /// the user reported. `request_render` fixes this for every trigger it covers; this test
    /// proves it for a category chip — a synchronous trigger, so (like the view-mode test above)
    /// the spinner's state can be asserted immediately after the tap, with no main-loop pump in
    /// between. Search goes through the exact same `request_render` call (see its debounce
    /// callback), but proving that specifically would need pumping the main loop to let the
    /// debounce timer fire first — and once that happens, the freshly-scheduled deferred render
    /// is dispatched in the very same GLib pass in this headless test harness (no real frame
    /// clock pacing it apart the way a live compositor would), so the intermediate state isn't
    /// reliably observable here even though the underlying code path is identical.
    pub(crate) fn run_category_chip_shows_a_spinner_while_regrouping(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Book By Weir", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Book By Herbert", "Frank Herbert", 1_600_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));
        assert!(!hooks.busy_spinner.is_visible());
        assert!(!hooks.busy_scrim.is_visible(), "the dimming scrim behind the spinner must hide with it");

        hooks.category_author.set_active(true);
        // Before pumping the main loop at all: same proof as the view-mode test above, this time
        // for a category chip — the spinner and the stale, ungrouped content must both still be
        // exactly as they were, ahead of the deferred regroup-and-rebuild.
        assert!(hooks.busy_spinner.is_visible(), "the spinner should appear before the regroup runs");
        assert!(hooks.busy_scrim.is_visible(), "the dimming scrim should appear together with the spinner");
        assert_eq!(flow_box_entries(&hooks.flow_box).len(), 2, "the stale, ungrouped entries should still be showing while the spinner is up");

        pump_until(|| flow_box_entries(&hooks.flow_box).len() == 4, Duration::from_secs(5));
        assert!(!hooks.busy_spinner.is_visible(), "the spinner should hide once the regroup is done");
        assert!(!hooks.busy_scrim.is_visible(), "the scrim should hide with it");

        // Switching back to "All" is the same code path (`category_all`'s own `connect_toggled`
        // → `set_grouping` → `request_render`) — regression coverage for the reported gap where
        // only the forward direction (All → a grouped category) was ever exercised.
        hooks.category_all.set_active(true);
        assert!(hooks.busy_spinner.is_visible(), "the spinner should appear switching back to All too");
        assert!(hooks.busy_scrim.is_visible(), "and the scrim with it");
        pump_until(|| flow_box_entries(&hooks.flow_box).len() == 2, Duration::from_secs(5));
        assert!(!hooks.busy_spinner.is_visible(), "the spinner should hide once back on All");
        assert!(!hooks.busy_scrim.is_visible(), "the scrim should hide with it");
    }

    pub(crate) fn run_list_view_rows_show_title_and_subtitle(runtime: &tokio::runtime::Runtime) {
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
                    "results": [item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0)]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));

        let row = hooks.list_box.row_at_index(0).unwrap().downcast::<adw::ActionRow>().unwrap();
        assert_eq!(row.title(), "Project Hail Mary");
        assert_eq!(row.subtitle().unwrap(), "Andy Weir · 1.0h");
    }

    pub(crate) fn run_tapping_a_list_row_invokes_on_open(runtime: &tokio::runtime::Runtime) {
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
                    "results": [item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0)]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let opened: Rc<std::cell::RefCell<Vec<PlayRequest>>> = Rc::new(std::cell::RefCell::new(Vec::new()));
        let on_open = {
            let opened = opened.clone();
            move |request: PlayRequest| opened.borrow_mut().push(request)
        };

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), on_open, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));

        let row = hooks.list_box.row_at_index(0).unwrap().downcast::<adw::ActionRow>().unwrap();
        row.emit_by_name::<()>("activated", &[]);

        assert_eq!(opened.borrow().len(), 1, "clicking the card should invoke on_open exactly once (opening Item Detail)");
        assert_eq!(opened.borrow()[0].item_id, "item-1");
    }

    pub(crate) fn run_search_and_sort_apply_in_list_mode_too(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Zed Book", "Author A", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Alpha Book", "Author B", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));

        assert_eq!(list_box_titles(&hooks.list_box), vec!["Zed Book", "Alpha Book"], "default sort is date-added descending");

        hooks.sort_by_row.set_selected(1); // Title
        pump_until(|| list_box_titles(&hooks.list_box) == vec!["Alpha Book".to_string(), "Zed Book".to_string()], Duration::from_secs(5));

        hooks.search_entry.set_text("zed");
        pump_until(|| list_box_titles(&hooks.list_box) == vec!["Zed Book".to_string()], Duration::from_secs(5));
    }

    /// Building a second screen against the same pool simulates the next time this tab is opened
    /// (same local DB, fresh widgets) — the persisted mode should apply itself without the user
    /// having to click the toggle again.
    pub(crate) fn run_view_mode_is_remembered_across_screen_rebuilds(runtime: &tokio::runtime::Runtime) {
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
                    "results": [item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0)]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let first_screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account.clone(), abs_core::auth::Session::new(pool.clone(), &server, &account), offline_mode.clone(), |_| {}, || {}, || {});
        let first_hooks = first_screen.test_hooks();
        pump_until(|| first_hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));
        assert!(first_hooks.list_box.is_visible(), "starts in list mode with nothing persisted yet");

        first_hooks.view_toggle.set_active(false);
        pump_until(|| first_hooks.flow_box.child_at_index(0).is_some(), Duration::from_secs(5));

        // The toggle's save is a fire-and-forget `spawn_future_local` (and the grid cards render
        // synchronously), so the cards appearing proves nothing about the write having committed.
        // Probe the persisted value on the same main context — a future queued after the save —
        // and only rebuild once it reads back Grid, so the second screen's load can't race the
        // first screen's save.
        let persisted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        glib::spawn_future_local({
            let pool = pool.clone();
            let persisted = persisted.clone();
            async move {
                loop {
                    if abs_core::settings::load_library_view_mode(&pool).await.ok() == Some(abs_core::settings::LibraryViewMode::Grid) {
                        persisted.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    glib::timeout_future(Duration::from_millis(20)).await;
                }
            }
        });
        pump_until(|| persisted.load(std::sync::atomic::Ordering::SeqCst), Duration::from_secs(5));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let second_screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let second_hooks = second_screen.test_hooks();
        pump_until(|| second_hooks.flow_box.child_at_index(0).is_some(), Duration::from_secs(10));

        assert!(second_hooks.flow_box.is_visible(), "a freshly built screen should restore the persisted Grid mode");
        assert!(!second_hooks.list_box.is_visible());
    }

    /// The fix for the reported freeze: typing must not synchronously re-render on every
    /// `search-changed` emission — only after a quiet window.
    pub(crate) fn run_search_is_debounced(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Dune", "Frank Herbert", 1_600_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));

        hooks.search_entry.set_text("dune");
        // Immediately after — well inside the 200ms debounce window — the render must not have
        // happened yet: still both items, not just the match.
        assert_eq!(flow_box_titles(&hooks.flow_box).len(), 2, "typing must not synchronously re-render");

        // Comfortably past the debounce window, the filtered result should have landed.
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Dune".to_string()], Duration::from_secs(5));

        // The actual reported crash: typing again *after* the first debounce already fired (so
        // its one-shot GLib source has already self-destroyed) used to panic inside
        // `SourceId::remove` — a panic that aborts the whole process, since it's raised from a
        // GTK signal handler. Regression guard for `crate::widgets::Debouncer`'s fix.
        hooks.search_entry.set_text("");
        pump_until(|| flow_box_titles(&hooks.flow_box).len() == 2, Duration::from_secs(5));
    }

    /// A real, distinct 1x1 PNG per item — enough for `CoverImage`'s decode path to succeed
    /// (content is sniffed, not extension-based), without needing real asset files in the repo.
    /// Mirrors `widgets::cover_image::tests::write_1x1_png`, duplicated rather than shared since
    /// that one is private to its own module and this is the only other place that needs it.
    fn write_1x1_png(path: &std::path::Path) {
        const PNG_1X1: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63,
            0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
            0x42, 0x60, 0x82,
        ];
        std::fs::write(path, PNG_1X1).unwrap();
    }

    /// Seeds a library of `count` items, each with a real (tiny) cached cover file, entirely
    /// locally (no wiremock — sync isn't needed for this test, only the local rows it reads).
    fn seed_items_with_real_covers(runtime: &tokio::runtime::Runtime, pool: &SqlitePool, server_id: &str, count: usize, cover_dir: &std::path::Path) {
        runtime.block_on(abs_storage::repo::libraries::upsert(
            pool,
            abs_storage::repo::libraries::UpsertLibrary { id: "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        ))
        .unwrap();
        for i in 0..count {
            let id = format!("item-{i}");
            let cover_path = cover_dir.join(format!("{id}.png"));
            write_1x1_png(&cover_path);
            let added_at = chrono::DateTime::from_timestamp_millis(1_700_000_000_000 - i as i64).unwrap();
            runtime
                .block_on(abs_storage::repo::items::upsert(
                    pool,
                    abs_storage::repo::items::UpsertItem {
                        id: &id,
                        server_id,
                        library_id: "e4bb1afb-4a4f-4dd6-8be0-e615d233185b",
                        title: &format!("Book {i}"),
                        author: None,
                        narrator: None,
                        description: None,
                        duration_seconds: 3600.0,
                        added_at,
                        series_name: None,
                        genres: &[],
                    },
                ))
                .unwrap();
            runtime.block_on(abs_storage::repo::items::set_cover_cache_path(pool, server_id, &id, Some(cover_path.to_str().unwrap()))).unwrap();
        }
    }

    /// The fix's other half: not every visible-in-principle item's cover decodes right away —
    /// only ones near the scrolled viewport. A tall, narrow window with many items means most
    /// of them start well below the fold and must still show their placeholder right after the
    /// initial render.
    pub(crate) fn run_deferred_decode_only_covers_items_near_the_viewport(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                // Lists the library seeded below, and fails its items: sync must leave the seeded
                // books alone (a library the server didn't list would be removed, with them).
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let cover_dir = tempfile::tempdir().unwrap();
        seed_items_with_real_covers(runtime, &pool, &server.id, 40, cover_dir.path());

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        let app_window = adw::ApplicationWindow::builder().default_width(300).default_height(400).build();
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        pump_until(|| hooks.list_box.row_at_index(39).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(39).is_some(), Duration::from_secs(10));
        // Give the deferred decode's idle callback, and whatever it kicks off, a real chance to
        // run and settle.
        pump_until(|| false, Duration::from_millis(500));

        let last_card = hooks.flow_box.child_at_index(39).expect("40 items were seeded");
        let last_button = last_card.child().and_then(|w| w.downcast::<gtk4::Button>().ok()).expect("card wraps a button");
        let last_cover_picture = last_button
            .child()
            .and_then(|card_box| card_box.first_child())
            .and_then(|cover_overlay| cover_overlay.first_child())
            .and_then(|w| w.downcast::<gtk4::Picture>().ok());
        assert!(
            last_cover_picture.is_none_or(|picture| !picture.is_visible()),
            "an item far below the fold should not have decoded its cover yet"
        );

        // Scroll all the way down and confirm the last item's cover eventually decodes.
        hooks.scroller.vadjustment().set_value(hooks.scroller.vadjustment().upper());
        pump_until(
            || {
                hooks
                    .flow_box
                    .child_at_index(39)
                    .and_then(|child| child.child())
                    .and_then(|w| w.downcast::<gtk4::Button>().ok())
                    .and_then(|button| button.child())
                    .and_then(|card_box| card_box.first_child())
                    .and_then(|cover_overlay| cover_overlay.first_child())
                    .and_then(|w| w.downcast::<gtk4::Picture>().ok())
                    .is_some_and(|picture| picture.is_visible())
            },
            Duration::from_secs(5),
        );

        // The actual reported crash: scrolling again *after* the first scroll's debounce already
        // fired (so its one-shot GLib source has already self-destroyed) used to panic inside
        // `SourceId::remove`, aborting the process. Regression guard for
        // `crate::widgets::Debouncer`'s fix — this must simply not crash.
        hooks.scroller.vadjustment().set_value(0.0);
        pump_until(|| false, Duration::from_millis(300));
    }

    /// The view-options popover's "Hide finished" switch (LB-8) — independent of "In progress
    /// only" above, and driven the same way `settings.rs`'s own switch tests are (`state-set`,
    /// not `set_active` — see that file's tests for why).
    pub(crate) fn run_hide_finished_switch_filters_finished_items(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Still Reading", "Author A", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Already Finished", "Author B", 1_600_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let now = chrono::Utc::now();
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-1", "Still Reading", 1_700_000_000_000, Some((600.0, false, now))));
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-2", "Already Finished", 1_600_000_000_000, Some((3600.0, true, now))));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));

        let _: bool = hooks.hide_finished_switch.emit_by_name("state-set", &[&true]);
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Still Reading".to_string()], Duration::from_secs(5));

        let _: bool = hooks.hide_finished_switch.emit_by_name("state-set", &[&false]);
        pump_until(|| flow_box_titles(&hooks.flow_box).len() == 2, Duration::from_secs(5));

        // The sheet's "Downloaded only" switch is the exact same shared state as the header's
        // own offline-mode toggle (see `build`'s doc comment on `downloaded_only_switch_handler`)
        // — flipping one must flip the other, same "one boolean, two widgets" proof the existing
        // cross-screen offline-mode test already gives Home vs. Library.
        assert!(!hooks.downloaded_only_switch.is_active());
        let _: bool = hooks.downloaded_only_switch.emit_by_name("state-set", &[&true]);
        pump_until(|| hooks.offline_toggle.is_active(), Duration::from_secs(5));
    }

    /// The Author category chip (LB-4/LB-9): activating it sets `Grouping::ByAuthor` — the same
    /// persisted value the popover's own "Grouping" combo drives — and the grid grows a section
    /// header per author. Not sticky (see this module's doc comment), just grouped and ordered.
    pub(crate) fn run_author_category_chip_groups_items_with_headers(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Book By Weir", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Book By Herbert", "Frank Herbert", 1_600_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        // A popover needs a mapped toplevel to show without crashing (`gtk_native_get_surface`
        // asserts otherwise) — same reasoning `run_sync_now_and_pull_to_refresh` documents for
        // its own popover.
        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));

        hooks.category_author.set_active(true);
        pump_until(|| flow_box_entries(&hooks.flow_box).len() == 4, Duration::from_secs(5));

        let entries = flow_box_entries(&hooks.flow_box);
        assert_eq!(
            entries,
            vec!["§Andy Weir".to_string(), "Book By Weir".to_string(), "§Frank Herbert".to_string(), "Book By Herbert".to_string()],
            "grouping by author should insert one alphabetically-ordered header per author"
        );

        // Regression guard for the real-device bug where a header forced the whole window wider
        // than the screen: `flow_box` is `.homogeneous(true)`, so a header with an oversized
        // `width_request`/`hexpand` balloons every tile's width, and the box's own natural-width
        // request, right along with it (see `grid_group_header`'s doc comment). Xvfb can't
        // reproduce the actual window-width overflow this caused on a real phone, so this checks
        // the underlying cause directly: the header must not carry a forced minimum width.
        let header = hooks
            .flow_box
            .child_at_index(0)
            .and_then(|child| child.child())
            .and_then(|w| w.downcast::<gtk4::Label>().ok())
            .expect("the first flow box child is the 'Andy Weir' header label");
        assert_eq!(header.width_request(), -1, "the header must not force a minimum width onto the homogeneous flow box");
        assert!(!header.hexpands(), "the header must not try to force itself onto its own line via hexpand");

        // The popover's own "Grouping" combo must reflect the chip's choice once it's opened —
        // the two are one persisted value, refreshed on `connect_visible_notify` (see `build`).
        hooks.view_options_button.popover().unwrap().set_visible(true);
        assert_eq!(hooks.grouping_row.selected(), 2, "the combo should show 'By Author' after the chip set it");
        hooks.view_options_button.popover().unwrap().set_visible(false);

        // List mode groups the same way, via a plain non-activatable `GtkListBoxRow` header.
        hooks.view_toggle.set_active(true);
        pump_until(|| list_box_entries(&hooks.list_box).len() == 4, Duration::from_secs(5));
        assert_eq!(
            list_box_entries(&hooks.list_box),
            vec!["§Andy Weir".to_string(), "Book By Weir".to_string(), "§Frank Herbert".to_string(), "Book By Herbert".to_string()]
        );

        // Regression guard for the real-device bug where this exact header, in list mode, still
        // forced the window wider than the screen after the grid-mode header was fixed (`list_box`
        // shares the same non-scrolling outer `scroller` as `flow_box` — see `list_group_header`'s
        // doc comment). Xvfb can't reproduce the actual overflow, so check the underlying cause
        // directly: the header label must be capped, not left to ask for its full natural width.
        let list_header = hooks
            .list_box
            .row_at_index(0)
            .and_then(|row| row.child())
            .and_then(|w| w.downcast::<gtk4::Label>().ok())
            .expect("the first list box row is the 'Andy Weir' header label");
        assert_eq!(list_header.max_width_chars(), 1, "the list-mode header must cap its natural width, same as the grid-mode header");
        assert_eq!(list_header.ellipsize(), gtk4::pango::EllipsizeMode::End);

        hooks.view_toggle.set_active(false);

        // The Series chip drives the same Cell via a different trigger — every item here has no
        // series, so it all falls into one "Other" bucket rather than one per book.
        hooks.category_series.set_active(true);
        pump_until(|| flow_box_entries(&hooks.flow_box) == vec!["§Other".to_string(), "Book By Weir".to_string(), "Book By Herbert".to_string()], Duration::from_secs(5));

        // Switching back to All must ungroup and clear the headers.
        hooks.category_all.set_active(true);
        pump_until(|| flow_box_entries(&hooks.flow_box).len() == 2, Duration::from_secs(5));
    }

    /// Regression test for the reported bug: grouping by Series with a realistic mix of items
    /// (some with real series metadata, some without) must not bury the real series among a
    /// dominant "Other" bucket — the fallback group must always sort last, and real series names
    /// must compare case-insensitively (`"apple"` vs `"Banana"` would sort "Banana" first under a
    /// plain byte-wise `String` sort, which is the wrong alphabetical order).
    pub(crate) fn run_series_grouping_sorts_named_groups_before_the_fallback_bucket(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json_with_series_and_genres("item-1", "Book B", Some("Banana"), &[]),
                        item_json_with_series_and_genres("item-2", "Book A", Some("apple"), &[]),
                        item_json_with_series_and_genres("item-3", "Book None", None, &[]),
                        // What Audiobookshelf actually sends for a book outside any series.
                        item_json_with_series_and_genres("item-4", "Book Blank", Some(""), &[])
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));
        // Library defaults to List view; switch to Grid so `flow_box_entries` sees anything.
        pump_until(|| hooks.list_box.row_at_index(3).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(3).is_some(), Duration::from_secs(10));

        hooks.category_series.set_active(true);
        pump_until(|| flow_box_entries(&hooks.flow_box).len() == 7, Duration::from_secs(5));

        let entries = flow_box_entries(&hooks.flow_box);
        assert_eq!(
            entries[..5],
            [
                "§apple".to_string(),
                "Book A".to_string(),
                "§Banana".to_string(),
                "Book B".to_string(),
                "§Other".to_string(),
            ],
            "named series should sort case-insensitively ahead of the fallback 'Other' bucket, \
             which must always land last regardless of where it would fall alphabetically"
        );
        let mut others = entries[5..].to_vec();
        others.sort();
        assert_eq!(others, ["Book Blank", "Book None"], "a blank series is no series: {entries:?}");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. What a screen reader gets here: a
    /// "Library" heading to land on, chips as radio buttons with the active one checked, one Tab
    /// stop per grid card (the flow box's own cell around it is skipped), and the number of
    /// books found on the search entry as the user types.
    pub(crate) fn run_chips_cards_and_search_speak_for_screen_readers(runtime: &tokio::runtime::Runtime) {
        use crate::a11y::tests::{description_of, focused_name, is_checked, label_of, role_of};
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
                    "results": [
                        item_json("item-1", "Book A", "Author One", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Book B", "Author One", 1_700_000_001_000, 3600.0),
                        item_json("item-3", "Other C", "Author Two", 1_700_000_002_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, || {});
        let hooks = screen.test_hooks();
        let window = gtk4::Window::builder().child(&screen.root).default_width(360).default_height(720).build();
        window.present();
        pump_until(|| hooks.list_box.row_at_index(2).is_some(), Duration::from_secs(10));
        pump_until(|| focused_name(&window) == "Library", Duration::from_secs(5));
        assert_eq!(focused_name(&window), "Library", "lands on the tab's heading, never the search entry");

        assert_eq!(role_of(&hooks.category_author), gtk4::AccessibleRole::Radio);
        assert!(is_checked(&hooks.category_all) && !is_checked(&hooks.category_author));
        hooks.category_author.set_active(true);
        assert!(is_checked(&hooks.category_author) && !is_checked(&hooks.category_all));
        hooks.category_all.set_active(true);

        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(2).is_some(), Duration::from_secs(10));
        let cell = hooks.flow_box.child_at_index(0).unwrap();
        assert!(!cell.is_focusable(), "the flow box's cell is not a Tab stop of its own");
        let card = cell.child().unwrap();
        assert!(card.is_focusable() && label_of(&card).is_some(), "the card inside it is");

        assert_eq!(description_of(&hooks.search_entry).as_deref(), Some("3 books"));
        hooks.search_entry.set_text("Book");
        pump_until(|| description_of(&hooks.search_entry).as_deref() == Some("2 books"), Duration::from_secs(5));
        hooks.search_entry.set_text("Other");
        pump_until(|| description_of(&hooks.search_entry).as_deref() == Some("1 book"), Duration::from_secs(5));
        assert_eq!(description_of(&hooks.search_entry).as_deref(), Some("1 book"));
        window.close();
    }

    /// Item Detail's series-button tap-through (`LibraryScreen::apply_series_filter`): filters to
    /// exactly one series by name, across two different series and a series-less item, without
    /// grouping headers (unlike the bare Series chip, which groups every series' books).
    pub(crate) fn run_apply_series_filter_shows_only_that_series(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json_with_series_and_genres("item-1", "Foundation Book", Some("Foundation"), &[]),
                        item_json_with_series_and_genres("item-2", "Dune Book", Some("Dune"), &[]),
                        item_json_with_series_and_genres("item-3", "Standalone Book", None, &[])
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));
        // Library defaults to List view; switch to Grid so `flow_box_entries` sees anything.
        pump_until(|| hooks.list_box.row_at_index(2).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(2).is_some(), Duration::from_secs(10));

        screen.apply_series_filter("Foundation");
        pump_until(|| flow_box_entries(&hooks.flow_box).len() == 1, Duration::from_secs(5));
        assert_eq!(flow_box_entries(&hooks.flow_box), vec!["Foundation Book".to_string()], "only the exact-matching series' item should show, with no grouping header");
    }

    /// The `ByAuthor` counterpart of the Series test above — items missing author metadata must
    /// fall into an "Unknown author" bucket that sorts last, not wherever it lands alphabetically.
    pub(crate) fn run_author_grouping_sorts_named_authors_before_unknown_author_bucket(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Book By Weir", "Andy Weir", 1_700_000_000_000, 3600.0),
                        serde_json::json!({
                            "id": "item-2",
                            "addedAt": 1_600_000_000_000i64,
                            "media": { "duration": 3600.0, "metadata": { "title": "Book With No Author" } }
                        }),
                        // What Audiobookshelf actually sends for a book without an author.
                        serde_json::json!({
                            "id": "item-3",
                            "addedAt": 1_500_000_000_000i64,
                            "media": { "duration": 3600.0, "metadata": { "title": "Book With Blank Author", "authorName": "" } }
                        })
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));
        // Library defaults to List view; switch to Grid so `flow_box_entries` sees anything.
        pump_until(|| hooks.list_box.row_at_index(2).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(2).is_some(), Duration::from_secs(10));

        hooks.category_author.set_active(true);
        pump_until(|| flow_box_entries(&hooks.flow_box).len() == 5, Duration::from_secs(5));

        assert_eq!(
            flow_box_entries(&hooks.flow_box),
            vec![
                "§Andy Weir".to_string(),
                "Book By Weir".to_string(),
                "§Unknown author".to_string(),
                "Book With No Author".to_string(),
                "Book With Blank Author".to_string(),
            ],
            "the 'Unknown author' fallback bucket must sort last, not wherever it lands alphabetically"
        );
    }

    /// The view-options popover's "Application settings" row (LB-11) — closes the popover and
    /// calls the screen's `on_open_settings` callback, the seam `main_window.rs` wires to
    /// `stack.set_visible_child_name("settings")`; this screen only owns the callback contract.
    pub(crate) fn run_application_settings_row_calls_on_open_settings(runtime: &tokio::runtime::Runtime) {
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
        let opened = Rc::new(Cell::new(false));
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, {
            let opened = opened.clone();
            move || opened.set(true)
        });
        let hooks = screen.test_hooks();

        // A popover needs a mapped toplevel to show without crashing — see
        // `run_author_category_chip_groups_items_with_headers`'s identical setup.
        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        hooks.view_options_popover.set_visible(true);
        adw::prelude::ActionRowExt::activate(&hooks.settings_row);
        assert!(opened.get(), "activating the row should call on_open_settings");
        assert!(!hooks.view_options_popover.is_visible(), "activating the row should also close the popover");
    }

    /// The persisted "Sort by" combo (LB-10) — drives the same `sort` Cell the header's own
    /// popover does, and survives a rebuild, same style as `run_view_mode_is_remembered_across_screen_rebuilds`.
    pub(crate) fn run_sort_by_combo_persists_and_is_honored_on_rebuild(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Zed Book", "Author A", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Alpha Book", "Author B", 1_600_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));

        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let first_screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account.clone(), abs_core::auth::Session::new(pool.clone(), &server, &account), offline_mode.clone(), |_| {}, || {}, || {});
        let first_hooks = first_screen.test_hooks();
        pump_until(|| first_hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        assert_eq!(list_box_titles(&first_hooks.list_box), vec!["Zed Book", "Alpha Book"], "default sort is date-added descending");

        first_hooks.sort_by_row.set_selected(1); // Title
        pump_until(|| list_box_titles(&first_hooks.list_box) == vec!["Alpha Book".to_string(), "Zed Book".to_string()], Duration::from_secs(5));

        let persisted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        glib::spawn_future_local({
            let pool = pool.clone();
            let persisted = persisted.clone();
            async move {
                loop {
                    if abs_core::settings::load_library_view_options(&pool).await.ok().map(|o| o.sort_by) == Some(abs_core::settings::SortBy::Title) {
                        persisted.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    glib::timeout_future(Duration::from_millis(20)).await;
                }
            }
        });
        pump_until(|| persisted.load(std::sync::atomic::Ordering::SeqCst), Duration::from_secs(5));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let second_screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let second_hooks = second_screen.test_hooks();
        pump_until(|| list_box_titles(&second_hooks.list_box) == vec!["Alpha Book".to_string(), "Zed Book".to_string()], Duration::from_secs(10));
    }

    /// The direct regression test for the old lossy `SortKey::to_sort_by` fallback this fix
    /// replaces: selecting "Last listened" via the sheet's combo used to silently persist as
    /// "Date of creation" instead (there was no `SortBy::LastListened` variant to persist it as),
    /// so a rebuilt screen would revert to date-added order. Now it's a real, lossless variant.
    pub(crate) fn run_last_listened_sort_persists_and_is_honored_on_rebuild(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Zed Book", "Author A", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Alpha Book", "Author B", 1_600_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let now = chrono::Utc::now();
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-1", "Zed Book", 1_700_000_000_000, Some((600.0, false, now - chrono::Duration::days(30)))));
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-2", "Alpha Book", 1_600_000_000_000, Some((600.0, false, now - chrono::Duration::days(1)))));

        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let first_screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account.clone(), abs_core::auth::Session::new(pool.clone(), &server, &account), offline_mode.clone(), |_| {}, || {}, || {});
        let first_hooks = first_screen.test_hooks();
        pump_until(|| first_hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));

        first_hooks.sort_by_row.set_selected(4); // Last listened
        pump_until(|| list_box_titles(&first_hooks.list_box) == vec!["Alpha Book".to_string(), "Zed Book".to_string()], Duration::from_secs(5));

        let persisted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        glib::spawn_future_local({
            let pool = pool.clone();
            let persisted = persisted.clone();
            async move {
                loop {
                    if abs_core::settings::load_library_view_options(&pool).await.ok().map(|o| o.sort_by) == Some(SortBy::LastListened) {
                        persisted.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    glib::timeout_future(Duration::from_millis(20)).await;
                }
            }
        });
        pump_until(|| persisted.load(std::sync::atomic::Ordering::SeqCst), Duration::from_secs(5));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let second_screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let second_hooks = second_screen.test_hooks();
        pump_until(|| list_box_titles(&second_hooks.list_box) == vec!["Alpha Book".to_string(), "Zed Book".to_string()], Duration::from_secs(10));

        // The combo's *displayed* selection is only refreshed on `connect_visible_notify` (see
        // `build`'s doc comment there), not kept live — so it must be opened once before
        // checking. A popover needs a mapped toplevel to show without crashing
        // (`gtk_native_get_surface` asserts otherwise) — same reasoning
        // `run_author_category_chip_groups_items_with_headers` documents for its own popover.
        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&second_screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));
        second_hooks.view_options_button.popover().unwrap().set_visible(true);
        assert_eq!(second_hooks.sort_by_row.selected(), 4, "the combo should show 'Last listened' after it was persisted");
    }

    /// "In progress only" moved here from the now-removed "Sort & filter" popover, where it used
    /// to be session-only — now the sheet's own switch persists it, like every other row here.
    pub(crate) fn run_in_progress_only_switch_persists_and_is_honored_on_rebuild(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json("item-1", "Reading Now", "Author A", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Untouched Book", "Author B", 1_600_000_000_000, 3600.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let now = chrono::Utc::now();
        runtime.block_on(seed_item_with_progress(&pool, &server.id, &account.id, "item-1", "Reading Now", 1_700_000_000_000, Some((600.0, false, now))));

        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let first_screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account.clone(), abs_core::auth::Session::new(pool.clone(), &server, &account), offline_mode.clone(), |_| {}, || {}, || {});
        let first_hooks = first_screen.test_hooks();
        pump_until(|| first_hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));

        let _: bool = first_hooks.in_progress_only_switch.emit_by_name("state-set", &[&true]);
        pump_until(|| list_box_titles(&first_hooks.list_box) == vec!["Reading Now".to_string()], Duration::from_secs(5));

        let persisted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        glib::spawn_future_local({
            let pool = pool.clone();
            let persisted = persisted.clone();
            async move {
                loop {
                    if abs_core::settings::load_library_view_options(&pool).await.ok().map(|o| o.in_progress_only) == Some(true) {
                        persisted.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    glib::timeout_future(Duration::from_millis(20)).await;
                }
            }
        });
        pump_until(|| persisted.load(std::sync::atomic::Ordering::SeqCst), Duration::from_secs(5));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let second_screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let second_hooks = second_screen.test_hooks();
        pump_until(|| list_box_titles(&second_hooks.list_box) == vec!["Reading Now".to_string()], Duration::from_secs(10));
        assert!(second_hooks.progress_banner.reveals_child(), "the filter banner should already be up since the filter was persisted active");
    }

    /// The Genre category chip (LB-4): picking a specific genre filters the list without
    /// grouping and without persisting anything — `LibraryViewOptions` has no genre field, and
    /// this is deliberately session-only (see `CategoryFilter`'s doc comment).
    pub(crate) fn run_genre_chip_filters_without_persisting(runtime: &tokio::runtime::Runtime) {
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
                    "results": [
                        item_json_with_series_and_genres("item-1", "Fantasy Book", None, &["Fantasy"]),
                        item_json_with_series_and_genres("item-2", "Sci-Fi Book", None, &["Sci-Fi"])
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool.clone(), crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));

        hooks.category_genre.set_active(true);
        pump_until(|| hooks.genre_chip_box.first_child().is_some(), Duration::from_secs(5));
        assert!(hooks.genre_chip_revealer.reveals_child(), "the genre row should reveal once the Genre chip is active");
        let chips = hooks.genre_chip_box.ancestor(gtk4::ScrolledWindow::static_type()).and_downcast::<gtk4::ScrolledWindow>().unwrap();
        assert!(crate::widgets::sideways_scroll::tests::passes_vertical_scroll_on(&chips), "the mouse wheel over the chips scrolls the list");

        let fantasy_chip = hooks
            .genre_chip_box
            .first_child()
            .and_then(|w| w.downcast::<gtk4::ToggleButton>().ok())
            .expect("first genre chip");
        assert_eq!(fantasy_chip.label().as_deref(), Some("Fantasy"), "genre chips should be sorted alphabetically");
        fantasy_chip.set_active(true);

        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Fantasy Book".to_string()], Duration::from_secs(5));

        let options = runtime.block_on(abs_core::settings::load_library_view_options(&pool)).unwrap();
        assert_eq!(options.grouping, abs_core::settings::Grouping::None, "a genre pick must never touch the persisted grouping");
    }

    /// End-to-end against the real public demo server, mirroring `welcome.rs`/`home.rs`'s live
    /// test tier. Not a `#[test]` itself — run via `main.rs`'s consolidated `#[ignore]`d entry
    /// point.
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
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(20));

        assert!(!hooks.status_page.is_visible(), "the live demo server has at least one item, so the empty state should clear");
        assert!(hooks.list_box.row_at_index(0).is_some());
    }

    /// "Sync now" via the ⋯ menu, then pull-to-refresh — the two manual triggers share one sync
    /// cycle and one outcome toast (LB-12, mirroring Home's HT-10 scenarios). Each trigger's
    /// round-trip lands a distinct response, so every stage's render is its own observable. The
    /// gesture itself is driven by the scroller's own `edge-overshot` signal — the exact one the
    /// gesture listens to; the sandbox has no touch hardware.
    pub(crate) fn run_sync_now_and_pull_to_refresh(runtime: &tokio::runtime::Runtime) {
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
                    "results": [item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0)]
                })))
                .up_to_n_times(1)
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Dune", "Frank Herbert", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .up_to_n_times(1)
                .mount(&mock_server),
        );
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Dune", "Frank Herbert", 1_600_000_000_000, 7200.0),
                        item_json("item-3", "Red Mars", "Kim Stanley Robinson", 1_500_000_000_000, 10800.0)
                    ]
                })))
                .mount(&mock_server),
        );

        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let hooks = screen.test_hooks();

        let app_window = adw::ApplicationWindow::builder().build();
        app_window.set_content(Some(&screen.root));
        // Mapped like Home's manual-sync scenarios — the outcome toast needs the overlay mapped.
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));
        assert_eq!(list_box_titles(&hooks.list_box).len(), 1);
        assert!(
            !crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Sync complete"),
            "the automatic cycle reports through the banner/status page, not a toast"
        );

        // Each trigger's round-trip lands a distinct response (1 → 2 → 3 items, via the stacked
        // `up_to_n_times` mocks above), so every stage's render is its own observable.
        hooks.sync_now_button.emit_clicked();
        // `ManualSync::claim` reveals the pull indicator synchronously, in the same handler
        // invocation as the click — no pump needed to observe it.
        assert!(hooks.pull_spinner.is_spinning(), "the pull indicator should reveal the instant a manual sync starts");
        pump_until(|| list_box_titles(&hooks.list_box).len() == 2, Duration::from_secs(10));
        // The toast lands at the cycle's resolve step — after its cover-fetch stage — so wait
        // on it, don't assert it immediately.
        pump_until(
            || crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Sync complete"),
            Duration::from_secs(10),
        );
        assert!(
            crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Sync complete"),
            "the manual sync's completion toast must appear"
        );
        assert!(!hooks.pull_spinner.is_spinning(), "the pull indicator should retract once the cycle resolves");

        hooks.scroller.emit_by_name::<()>("edge-overshot", &[&gtk4::PositionType::Top]);
        assert!(hooks.pull_spinner.is_spinning(), "the pull indicator should reveal for the gesture trigger too");
        pump_until(|| list_box_titles(&hooks.list_box).len() == 3, Duration::from_secs(10));
        // Waiting on the toast text alone would race the first toast, which can still be showing
        // (it hasn't timed out yet) — so it'd already read true before this cycle's own `finish`
        // runs. The pull indicator has no such ambiguity: it's only ever unset by `finish`.
        pump_until(|| !hooks.pull_spinner.is_spinning(), Duration::from_secs(10));
        assert!(
            crate::test_support::any_label_reads(hooks.toast_overlay.upcast_ref(), "Sync complete"),
            "the pull's completion toast must appear too"
        );
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A big library is built in slices so the
    /// main loop is never held for the whole of it, a newer render abandons an older one still
    /// being built, and a render that would show exactly what is already there builds nothing.
    pub(crate) fn run_a_big_library_renders_in_slices_and_skips_identical_renders(runtime: &tokio::runtime::Runtime) {
        const BOOKS: usize = 130;
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "libraries": [{ "id": "e4bb1afb-4a4f-4dd6-8be0-e615d233185b", "name": "Audiobooks", "mediaType": "book" }]
                })))
                .mount(&mock_server),
        );
        let results: Vec<_> = (0..BOOKS).map(|i| item_json(&format!("item-{i:03}"), &format!("Book {i:03}"), "An Author", 1_700_000_000_000 + i as i64, 3600.0)).collect();
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/api/libraries/e4bb1afb-4a4f-4dd6-8be0-e615d233185b/items"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": results })))
                .mount(&mock_server),
        );
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, || {});
        let hooks = screen.test_hooks();
        let rows = || {
            let mut count = 0;
            while hooks.list_box.row_at_index(count).is_some() {
                count += 1;
            }
            count as usize
        };

        pump_until(|| rows() == BOOKS, Duration::from_secs(20));
        assert_eq!(rows(), BOOKS, "every book ends up on screen");

        // Applying the view that is already shown builds nothing: the very same rows stay.
        screen.apply_view(SortKey::DateAdded, false);
        pump_until(|| false, Duration::from_millis(500));
        pump_until(|| rows() == BOOKS, Duration::from_secs(20));
        let first_row = hooks.list_box.row_at_index(0).unwrap();
        screen.apply_view(SortKey::DateAdded, false);
        pump_until(|| false, Duration::from_millis(300));
        assert_eq!(rows(), BOOKS);
        assert_eq!(hooks.list_box.row_at_index(0).as_ref(), Some(&first_row), "an identical render must not rebuild the list");

        // A filter typed straight after another render began wins: nothing of the abandoned
        // build is left behind.
        screen.apply_view(SortKey::Title, false);
        hooks.search_entry.set_text("book 129");
        pump_until(|| rows() == 1, Duration::from_secs(10));
        pump_until(|| false, Duration::from_millis(500));
        assert_eq!(rows(), 1, "only the match remains");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The Library's badges follow downloads
    /// finishing and being cleared in this session, and offline mode's list gains the book.
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
                    "results": [
                        item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0),
                        item_json("item-2", "Dune", "Frank Herbert", 1_600_000_000_000, 7200.0)
                    ]
                })))
                .mount(&mock_server),
        );
        let pool = runtime.block_on(pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let offline_mode = crate::offline_mode::OfflineModeState::new(pool.clone());
        let screen = build(pool.clone(), crate::test_support::test_paths(), server.clone(), account, session, offline_mode.clone(), |_| {}, || {}, || {});
        let download_manager =
            crate::downloads::DownloadManager::new(pool.clone(), crate::test_support::test_paths(), Box::new(abs_player::network_watch::UnknownNetworkMonitor), false);
        screen.follow_downloads(&download_manager);
        let hooks = screen.test_hooks();
        pump_until(|| hooks.list_box.row_at_index(1).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(1).is_some(), Duration::from_secs(10));
        let badges = || {
            let mut shown = 0;
            let mut index = 0;
            while let Some(child) = hooks.flow_box.child_at_index(index) {
                if child.child().is_some_and(|card| crate::widgets::item_card::tests::downloaded_badge_of(&card).is_visible()) {
                    shown += 1;
                }
                index += 1;
            }
            shown
        };
        assert_eq!(badges(), 0);

        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-2", &[abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 7200.0, offset_seconds: 0.0, size_bytes: None }])).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::upsert_pending(&pool, &server.id, "item-2", "1", "/p/1.mp3")).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::mark_complete(&pool, &server.id, "item-2", "1", 10)).unwrap();
        download_manager.publish_for_test(crate::downloads::DownloadEvent::ItemStateChanged { item_id: "item-2".to_string(), state: crate::downloads::ItemDownloadState::Complete });
        pump_until(|| badges() == 1, Duration::from_secs(5));
        assert_eq!(badges(), 1, "the finished download shows its badge without a sync");

        offline_mode.set(true);
        pump_until(|| flow_box_titles(&hooks.flow_box) == vec!["Dune".to_string()], Duration::from_secs(5));
        assert_eq!(flow_box_titles(&hooks.flow_box), vec!["Dune".to_string()], "offline mode shows the newly downloaded book");

        runtime.block_on(abs_storage::repo::download_tracks::remove_for_item(&pool, &server.id, "item-2")).unwrap();
        download_manager.publish_for_test(crate::downloads::DownloadEvent::ItemStateChanged { item_id: "item-2".to_string(), state: crate::downloads::ItemDownloadState::Idle });
        pump_until(|| hooks.flow_box.child_at_index(0).is_none(), Duration::from_secs(5));
        assert!(hooks.flow_box.child_at_index(0).is_none(), "a cleared download leaves offline mode's list");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Switching between grid and list leaves
    /// nothing behind in the container no longer shown (it used to keep its whole widget tree), and
    /// low memory mode turns the covers on the grid into placeholders and back.
    pub(crate) fn run_low_memory_mode_and_view_switches_release_what_is_not_shown(runtime: &tokio::runtime::Runtime) {
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
                    "results": [item_json("item-1", "Project Hail Mary", "Andy Weir", 1_700_000_000_000, 3600.0)]
                })))
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
        let screen = build(pool, crate::test_support::test_paths(), server, account, session, offline_mode, |_| {}, || {}, || {});
        screen.follow_low_memory_mode(&low_memory_mode);
        let hooks = screen.test_hooks();

        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));
        hooks.view_toggle.set_active(false);
        pump_until(|| hooks.flow_box.child_at_index(0).is_some(), Duration::from_secs(10));
        assert!(hooks.list_box.row_at_index(0).is_none(), "the list is emptied while the grid is shown");
        let cover_showing = || {
            hooks
                .flow_box
                .child_at_index(0)
                .and_then(|child| child.child())
                .and_then(|card| crate::widgets::item_card::tests::cover_picture_of(&card))
                .is_some_and(|picture| picture.is_visible())
        };
        pump_until(cover_showing, Duration::from_secs(10));
        assert!(cover_showing(), "the grid shows the cover");

        low_memory_mode.set(true);
        pump_until(|| !cover_showing(), Duration::from_secs(5));
        assert!(!cover_showing(), "low memory mode: a placeholder");
        low_memory_mode.set(false);
        pump_until(cover_showing, Duration::from_secs(10));
        assert!(cover_showing(), "and the cover is back");

        hooks.view_toggle.set_active(true);
        pump_until(|| hooks.list_box.row_at_index(0).is_some(), Duration::from_secs(10));
        assert!(hooks.flow_box.child_at_index(0).is_none(), "the grid is emptied while the list is shown");
    }
}
