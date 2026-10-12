//! The Item Detail screen — `docs/design/ui-spec.md` §3's "Item detail" section and
//! `docs/ui-test-plan.md` §6 (ID-1 through ID-16): cover, metadata, progress, series info (name
//! and position — "Name, 5/8" — with a tap-through to Library filtered to that series, via
//! `on_open_series`; see `abs_core::series` for where the real numbers come from), a primary
//! Play/Resume button, the shared download-scope menu (`widgets::download_scope_menu`), a
//! truncated description, a tappable chapter list, and — at the bottom, same as Home/Library —
//! the mini bar reflecting whatever is currently playing. Pushed by `main_window` swapping window
//! content in — same content-swap mechanism `screens::player` already uses (there's no
//! `AdwNavigationView`/`AdwDialog` at this crate's libadwaita `v1_2` ceiling) — when a cover card
//! on Home/Library is tapped; the back button calls `on_back` to swap the shell back in.
//!
//! Restores the spec's real "tap cover -> Item Detail -> Play" flow: `widgets::item_card` used to
//! start playback directly (see its git history), a stand-in for this screen not existing yet.
//! Player itself stays a separate screen, reached only from the mini-bar, per the ui-spec's
//! navigation model — this screen never touches `abs-player`/`PlayerController` directly; starting
//! playback is owned entirely by the caller (`main_window`) via `on_play`, the same "screens report
//! intent, the shell acts on it" shape `home.rs`/`library.rs` already use for their own `on_open`.
//! `on_play` is also where navigation away from this screen happens on the Play/Resume and
//! chapter-tap paths — the caller's `on_play` opens the full Player screen directly, so this screen
//! never calls `on_back` itself except from its own header back button.
//!
//! Renders in two passes, same "show now, refine once the async bit lands" shape `home.rs`'s cover
//! art and `screens::player`'s chapters popover already use: metadata + progress come from a
//! local-only DB read (item + progress rows, already synced by the time a card is tappable) and
//! render first; chapters/tracks need `abs_core::streaming::resolve_stream_target` (the same call
//! Player's own `start()` makes), so the chapter list and the download-scope menu (which needs the
//! final chapter ranges) both arrive a beat later — falling back to whatever chapters/tracks are
//! already cached locally if the server can't be reached, so the page still works offline.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use abs_storage::models::{Account, Server};

use crate::downloads::DownloadManager;
use crate::i18n::{ntr_args, tr, tr_args};
use crate::widgets::cover_image::CoverImage;
use crate::widgets::download_scope_menu;
use crate::widgets::item_options_menu;

pub struct ItemDetailScreen {
    pub root: gtk4::Widget,
    #[cfg(test)]
    hooks: TestHooks,
}

#[cfg(test)]
pub struct TestHooks {
    pub back_button: gtk4::Button,
    pub title_label: gtk4::Label,
    pub author_label: gtk4::Label,
    pub series_button: gtk4::Button,
    pub series_button_label: gtk4::Label,
    pub duration_label: gtk4::Label,
    pub progress_bar: gtk4::ProgressBar,
    pub play_button: gtk4::Button,
    pub description_label: gtk4::Label,
    pub more_button: gtk4::Button,
    pub chapters_list: gtk4::ListBox,
    pub download_button: gtk4::MenuButton,
    pub download_popover: gtk4::Popover,
    pub download_popover_box: gtk4::Box,
    pub mark_as_finished_button: gtk4::Button,
    pub reset_progress_button: gtk4::Button,
    pub mini_bar: crate::player::MiniPlayerHooks,
    pub download_progress_revealer: gtk4::Revealer,
    pub cover: crate::widgets::cover_image::CoverImage,
}

#[cfg(test)]
impl ItemDetailScreen {
    pub fn test_hooks(&self) -> &TestHooks {
        &self.hooks
    }
}

/// Builds the screen for `item_id`. `on_play` fires when Play/Resume or a chapter row is tapped —
/// `None` for Play/Resume (resume from whatever progress the player itself resolves, same as
/// today), `Some(index)` for a chapter tap (seek to that chapter once playback starts). Neither
/// case calls `on_back` afterward: the caller's `on_play` is expected to take over navigation
/// itself (opening the full Player screen), not hand it back to this screen. `on_back` is used
/// only by the header's own back button.
///
/// `controller` is the shell's already-live `PlayerController`, shared (not duplicated) with the
/// mini bar this screen shows at the bottom — see `crate::player::build_mini_bar_for`'s doc for
/// why this reflects real playback state rather than a second, independent copy of it. Tapping or
/// swiping up on that mini bar calls `on_open_player`, exactly mirroring the shell's own mini
/// bar's tap/swipe-up-to-open gesture (`main_window::mini_bar_gesture_should_open`). `on_open_series`
/// fires when the series button (below the author line) is tapped, with the item's plain series
/// name — the caller is expected to close this screen and land on Library filtered to that series.
#[allow(clippy::too_many_arguments)]
pub fn build(
    pool: SqlitePool,
    server: Server,
    account: Account,
    session: abs_core::auth::Session,
    download_manager: DownloadManager,
    controller: crate::player::PlayerController,
    item_id: String,
    on_play: impl Fn(String, Option<usize>) + 'static,
    on_back: impl Fn() + 'static,
    on_open_player: impl Fn() + 'static,
    on_open_series: impl Fn(String) + 'static,
    on_open_downloads: impl Fn() + 'static,
) -> ItemDetailScreen {
    let _slow = crate::perf::SlowJob::new("item detail build");
    let on_play: Rc<dyn Fn(String, Option<usize>)> = Rc::new(on_play);
    let on_open_series = Rc::new(on_open_series);
    let on_back = Rc::new(on_back);
    let on_open_downloads: Rc<dyn Fn()> = Rc::new(on_open_downloads);

    let header = adw::HeaderBar::new();
    let back_button = gtk4::Button::from_icon_name("go-previous-symbolic");
    back_button.connect_clicked({
        let on_back = on_back.clone();
        move |_| on_back()
    });
    header.pack_start(&back_button);
    header.set_title_widget(Some(&adw::WindowTitle::new("", "")));

    // Set below, once `toast_overlay`/`options_menu` exist — `header.pack_end` doesn't care when
    // it's called relative to `header`'s own construction, only that it happens before `build`
    // returns.

    let cover = CoverImage::new(220);
    cover.widget().set_halign(gtk4::Align::Center);
    cover.widget().set_margin_top(14);
    // The title shown in the viewer, set once the item row is read below.
    let cover_title = Rc::new(RefCell::new(String::new()));
    cover.set_on_activate({
        let anchor = cover.widget().clone();
        let session = session.clone();
        let paths = controller.paths();
        let server_id = server.id.clone();
        let item_id = item_id.clone();
        let cover_title = cover_title.clone();
        move |shown, thumbnail| {
            crate::widgets::cover_viewer::open(
                &anchor,
                crate::widgets::cover_viewer::CoverSource {
                    session: session.clone(),
                    paths: paths.clone(),
                    server_id: server_id.clone(),
                    item_id: item_id.clone(),
                    title: cover_title.borrow().clone(),
                    thumbnail,
                    shown,
                },
            );
        }
    });

    // `max_width_chars(1)` caps each label's natural width regardless of `wrap` — this content
    // sits in a `ScrolledWindow` with `hscrollbar_policy(Never)` (see below), so an unclamped,
    // fully server-controlled title/author string could otherwise force the whole window wider
    // than the screen (see `widgets::banner`'s identical fix for the same reasoning).
    let title_label = gtk4::Label::builder().wrap(true).max_width_chars(1).justify(gtk4::Justification::Center).css_classes(["title-2"]).margin_top(18).selectable(true).build();
    // Selectable so the title can be copied (long-press or drag, then Copy), but never focusable:
    // a selectable label is focusable by default, and the first focusable label in a freshly
    // shown page gets its whole text selected — the title would open highlighted. Must come after
    // `selectable`, which turns focusability back on.
    title_label.set_focusable(false);
    let author_label = gtk4::Label::builder().wrap(true).max_width_chars(1).justify(gtk4::Justification::Center).css_classes(["dim-label"]).margin_top(4).visible(false).build();
    // One widget, whose *text* changes in place from the plain series name (Pass 1, local) to
    // "Name, N/M" (Pass 2, once the network call resolves) — never swapped for a different widget
    // and never allowed to wrap to a second line (`max_width_chars` + `ellipsize` on the label,
    // same device `title_label`/`author_label` above use), so filling in the numbers later can't
    // shift the chapters list/mini bar below it. Hidden entirely when the item has no series.
    let series_button = gtk4::Button::builder().css_classes(["flat"]).halign(gtk4::Align::Center).margin_top(4).visible(false).build();
    let series_button_label = gtk4::Label::builder().max_width_chars(1).ellipsize(gtk4::pango::EllipsizeMode::End).build();
    series_button.set_child(Some(&series_button_label));
    let duration_label = gtk4::Label::builder().css_classes(["caption", "dim-label"]).margin_top(4).build();

    let progress_bar = gtk4::ProgressBar::builder().margin_top(10).visible(false).build();

    // Invisible (but holding its place, so nothing shifts) and untappable until Pass 1 below has
    // read this book's progress: shown straight away it read "Play" for a frame or two before
    // turning into "Resume" on a started book.
    let play_button = gtk4::Button::builder()
        .label(tr("Play"))
        .css_classes(["pill", "suggested-action"])
        .halign(gtk4::Align::Center)
        .opacity(0.0)
        .sensitive(false)
        .build();

    let toast_overlay = adw::ToastOverlay::new();

    // The 14px gap above the actions row lives on the row itself, never on one of its children: a
    // one-sided child margin still adds to the row's height, which the (margin-less) download
    // button — a default Fill-valign child — then stretches to fill, making it taller than the
    // pill beside it. With the spacing here instead, the row's height is just the tallest child's
    // natural height and both buttons are allocated exactly that.
    let actions_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(12).halign(gtk4::Align::Center).margin_top(14).build();
    actions_row.append(&play_button);

    // Description: truncated text (ui-spec: `AdwExpanderRow` or plain text block, whichever is
    // less code — a plain label + "more" toggle here) with a "more" affordance that expands the
    // full text (ID-4). Hidden entirely when the item has no description at all.
    let description_label = gtk4::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .lines(3)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .margin_top(4)
        .build();
    let more_button = gtk4::Button::builder().label(tr("more")).css_classes(["flat"]).halign(gtk4::Align::Start).visible(false).build();
    let description_section = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).margin_top(16).visible(false).build();
    description_section.append(&gtk4::Label::builder().label(tr("Description")).xalign(0.0).css_classes(["heading"]).build());
    description_section.append(&description_label);
    description_section.append(&more_button);

    let expanded = Rc::new(Cell::new(false));
    more_button.connect_clicked({
        let expanded = expanded.clone();
        let description_label = description_label.clone();
        let more_button = more_button.clone();
        move |_| {
            let now_expanded = !expanded.get();
            expanded.set(now_expanded);
            if now_expanded {
                description_label.set_lines(-1);
                description_label.set_ellipsize(gtk4::pango::EllipsizeMode::None);
                more_button.set_label(&tr("less"));
            } else {
                description_label.set_lines(3);
                description_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                more_button.set_label(&tr("more"));
            }
        }
    });

    // Chapters (ui-spec: `AdwActionRow`s, tap to seek) — populated once chapters resolve (see
    // below); a "downloaded" legend sits in the section header (ID-15) so the offline glyph's
    // meaning doesn't need to be inferred from a single unlabeled icon.
    let chapters_legend = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(4).margin_bottom(4).build();
    chapters_legend.append(&gtk4::Image::builder().icon_name("object-select-symbolic").css_classes(["dim-label"]).build());
    chapters_legend.append(&gtk4::Label::builder().label(tr("downloaded")).css_classes(["caption", "dim-label"]).build());
    let chapters_list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    let chapters_section = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).margin_top(16).visible(false).build();
    chapters_section.append(&gtk4::Label::builder().label(tr("Chapters")).xalign(0.0).css_classes(["heading"]).margin_bottom(4).build());
    chapters_section.append(&chapters_legend);
    chapters_section.append(&chapters_list);

    let content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).margin_start(20).margin_end(20).margin_bottom(24).build();
    content.append(cover.widget());
    content.append(&title_label);
    content.append(&author_label);
    content.append(&series_button);
    content.append(&duration_label);
    content.append(&progress_bar);
    content.append(&actions_row);
    content.append(&description_section);
    content.append(&chapters_section);

    let scroller = gtk4::ScrolledWindow::builder().hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).child(&content).build();

    // The mini bar, at the bottom, same as Home/Library — bound to the shell's already-live
    // controller (see this function's own doc comment), not a second independent one. Tap or
    // swipe-up opens the full player, reusing the shell's own gesture decision exactly.
    let mini_bar = crate::player::build_mini_bar_for(controller.clone());
    let mini_bar_gesture = gtk4::GestureDrag::new();
    mini_bar_gesture.connect_drag_end({
        let on_open_player = Rc::new(on_open_player);
        move |gesture, _, _| {
            if let Some((offset_x, offset_y)) = gesture.offset() {
                if crate::screens::main_window::mini_bar_gesture_should_open(offset_x, offset_y) {
                    on_open_player();
                }
            }
        }
    });
    mini_bar.root.add_controller(mini_bar_gesture);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&scroller);
    root.append(&mini_bar.root);
    toast_overlay.set_child(Some(&root));

    // The download menu is built right away (synchronously, below), not once chapters resolve —
    // `download_scope_menu::build` asks its `chapter_ranges`/`current_chapter_index` fresh on
    // every popover open (see its own doc), so this page can hand it a shared cell that starts
    // empty/`0` and is filled in once pass 2 (chapters, over the network) lands, rather than
    // rebuilding or swapping the widget itself. That keeps a single, stable `MenuButton` for the
    // widget's whole lifetime — simpler for both the layout and `TestHooks`, which grabs it once.
    let chapter_ranges_cell: Rc<std::cell::RefCell<Vec<(f64, f64)>>> = Rc::new(std::cell::RefCell::new(Vec::new()));
    // False until `chapter_ranges_cell` actually reflects something real — either Pass 1's cache
    // seed or Pass 2's resolve, whichever lands first (see both below). Before that, an empty
    // `chapter_ranges_cell` is indistinguishable from "this book genuinely has no chapters",
    // which every scope estimate treats as "the whole book" — opening the download menu in that
    // gap used to show every option at the full-book size with "Current chapter" still tappable.
    // See `download_scope_menu::build`'s own doc for the report this fixes.
    let chapters_ready_cell: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // The chapter the resume position falls in, or 0 before it's known / for an unstarted book.
    let current_chapter_index_cell: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    // The item's duration, filled in by Pass 1 below — needed by the "Mark as finished" action
    // (which records progress at full duration) when this item isn't the one currently loaded
    // into `controller`, so there's no live playback duration to ask instead.
    let duration_seconds_cell: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
    // Chapters + the resume position, both filled in by Pass 2 below and kept around (this page's
    // chapter list never changes shape after that, per `chapter_ranges_cell`'s own doc above) so
    // `refresh_chapter_rows` can rebuild the "Downloaded" glyphs later without redoing Pass 2's
    // network resolve — only re-querying which chapters are downloaded, which does change.
    let chapters_cell: Rc<std::cell::RefCell<Vec<(String, f64, f64)>>> = Rc::new(std::cell::RefCell::new(Vec::new()));
    let progress_seconds_cell: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
    let download_menu = download_scope_menu::build(
        pool.clone(),
        download_manager.clone(),
        session.clone(),
        item_id.clone(),
        {
            let chapter_ranges_cell = chapter_ranges_cell.clone();
            move || chapter_ranges_cell.borrow().clone()
        },
        {
            let chapters_ready_cell = chapters_ready_cell.clone();
            move || chapters_ready_cell.get()
        },
        {
            let current_chapter_index_cell = current_chapter_index_cell.clone();
            move || current_chapter_index_cell.get()
        },
        {
            let download_manager = download_manager.clone();
            move || download_manager.free_space_bytes()
        },
        toast_overlay.clone(),
        on_open_downloads.clone(),
    );
    // Same `pill` treatment as the primary button, so the row reads as one pair of matched
    // controls (the shared widget keeps its plain icon-button look in Player's row — this is a
    // per-call-site style, and the widget is built fresh here).
    download_menu.widget.add_css_class("pill");
    actions_row.append(&download_menu.widget);

    // Visible progress for this item's own in-flight download (see
    // `widgets::download_progress`'s doc for why the button's static spinner icon alone wasn't
    // enough) — placed directly below the actions row, the same slot Home/Library use for their
    // pull-to-refresh indicator.
    // `insert_child_after`, not `append`: `content`'s children were already assembled above
    // (`actions_row` included) before this widget exists, so appending now would land it after
    // the chapters section instead of right below the actions row.
    let (progress_strip_widget, _progress_strip) =
        crate::widgets::download_progress::DownloadProgressStrip::build(download_manager.clone(), server.id.clone(), item_id.clone());
    content.insert_child_after(&progress_strip_widget, Some(&actions_row));

    // Keeps the chapter list's "Downloaded" glyphs live: Pass 2 above computes them once, but a
    // download can complete (or start, or fail) while this screen is still open — without this,
    // the glyphs stayed exactly as Pass 2 first found them until the screen was reopened. Refetch
    // is cheap (one indexed query over this item's chapter ranges), so this re-derives markers on
    // every event rather than trying to track "did a track just finish" precisely.
    download_manager.add_scoped_listener({
        let pool = pool.clone();
        let server_id = server.id.clone();
        let item_id = item_id.clone();
        let chapters_list = chapters_list.clone();
        let chapters_cell = chapters_cell.clone();
        let chapter_ranges_cell = chapter_ranges_cell.clone();
        let progress_seconds_cell = progress_seconds_cell.clone();
        let on_play = on_play.clone();
        move |event| {
            // This screen is built afresh per visit; once it is gone its list has no parent any
            // more, and the listener goes with it.
            if chapters_list.parent().is_none() {
                return false;
            }
            let event_item_id = match event {
                crate::downloads::DownloadEvent::ItemStateChanged { item_id, .. } => item_id,
                crate::downloads::DownloadEvent::TrackProgress { item_id, .. } => item_id,
            };
            if *event_item_id != item_id {
                return true;
            }
            let chapters = chapters_cell.clone();
            let chapter_ranges = chapter_ranges_cell.borrow().clone();
            if chapter_ranges.is_empty() {
                // Pass 2 hasn't landed yet (or this item has no chapters at all) — nothing to
                // refresh against.
                return true;
            }
            let pool = pool.clone();
            let server_id = server_id.clone();
            let item_id = item_id.clone();
            let chapters_list = chapters_list.clone();
            let progress_seconds_cell = progress_seconds_cell.clone();
            let on_play = on_play.clone();
            glib::spawn_future_local(async move {
                let markers = abs_core::download_tracks::chapter_offline_markers_for_item(&pool, &server_id, &item_id, &chapter_ranges).await.unwrap_or_default();
                refresh_chapter_rows(&chapters_list, &chapters.borrow(), progress_seconds_cell.get(), &markers, &on_play, &item_id);
            });
            true
        }
    });

    // "Mark as finished"/"Reset progress" — the same shared widget Player uses (see
    // `widgets::item_options_menu`'s doc), so both screens look and behave identically. Unlike
    // Player, this screen's `item_id` isn't necessarily the one `controller` currently has
    // loaded (the user could be browsing this book's details while something else plays, or
    // nothing at all) — `controller.current_item_id()` decides which of the two write paths
    // below applies: the live controller (also pauses/seeks real playback, exactly like Player)
    // when it matches, or a direct local-write-plus-best-effort-server-push
    // (`write_item_progress`) otherwise. Either path updates this
    // screen's own `play_button`/`progress_bar` immediately — both actions converge on the same
    // "Play", no progress bar" state Pass 1 below already computes for an unstarted/finished
    // item, so there's no need to wait on the async write to know what to show.
    let progress_action = ProgressAction {
        controller: controller.clone(),
        pool: pool.clone(),
        session: session.clone(),
        account_id: account.id.clone(),
        server_id: server.id.clone(),
        item_id: item_id.clone(),
        duration_seconds: duration_seconds_cell.clone(),
        progress_seconds: progress_seconds_cell.clone(),
        play_button: play_button.clone(),
        progress_bar: progress_bar.clone(),
        toast_overlay: toast_overlay.clone(),
    };
    let options_menu = item_options_menu::build(
        {
            let action = progress_action.clone();
            move || action.apply(&tr("Marked as finished"), &tr("Marking as finished"), |duration| (duration, true))
        },
        {
            let action = progress_action.clone();
            move || action.apply(&tr("Progress reset"), &tr("Resetting progress"), |_| (0.0, false))
        },
    );
    header.pack_end(&options_menu.widget);

    glib::spawn_future_local({
        let pool = pool.clone();
        let title_label = title_label.clone();
        let author_label = author_label.clone();
        let series_button = series_button.clone();
        let series_button_label = series_button_label.clone();
        let on_open_series = on_open_series.clone();
        let duration_label = duration_label.clone();
        let progress_bar = progress_bar.clone();
        let play_button = play_button.clone();
        let description_label = description_label.clone();
        let description_section = description_section.clone();
        let more_button = more_button.clone();
        let chapters_list = chapters_list.clone();
        let chapters_section = chapters_section.clone();
        let session = session.clone();
        let item_id = item_id.clone();
        let on_play = on_play.clone();
        let chapter_ranges_cell = chapter_ranges_cell.clone();
        let chapters_ready_cell = chapters_ready_cell.clone();
        let current_chapter_index_cell = current_chapter_index_cell.clone();
        let duration_seconds_cell = duration_seconds_cell.clone();
        let chapters_cell = chapters_cell.clone();
        let progress_seconds_cell = progress_seconds_cell.clone();
        let download_menu = download_menu.clone();
        let cover = cover.clone();
        async move {
            let server_id = server.id.clone();
            let account_id = account.id.clone();

            // Pass 1: metadata + progress — both a plain local DB read, so this renders
            // immediately (no network wait). Falls back to the id itself as a title and "Play"
            // if the item somehow isn't cached (shouldn't happen: a card is only tappable once
            // its item is synced), rather than leaving the page blank.
            let item = abs_storage::repo::items::get(&pool, &server_id, &item_id).await.ok();
            title_label.set_label(item.as_ref().map(|i| i.title.as_str()).unwrap_or(&item_id));
            if let Some(item) = &item {
                if let Some(author) = &item.author {
                    let subtitle = match item.narrator.as_deref().filter(|n| !n.is_empty()) {
                        Some(narrator) => {
                            // TRANSLATORS: {author} is the book's author, {narrator} the person who reads the audiobook aloud.
                            tr_args("{author} · Narrated by {narrator}", &[("author", author.as_str()), ("narrator", narrator)])
                        }
                        None => author.clone(),
                    };
                    author_label.set_label(&subtitle);
                    author_label.set_visible(true);
                }
                // Series: shown immediately with just the plain name (already cached on the
                // item itself, no network needed); Pass 2 below fills the real "N/M" into this
                // same label once the series-list call resolves. Clickable right away — jumping
                // to Library filtered by name doesn't need the numbers.
                if let Some(series_name) = item.series_name.as_deref().filter(|s| !s.is_empty()) {
                    series_button_label.set_label(series_name);
                    series_button.set_visible(true);
                    series_button.connect_clicked({
                        let on_open_series = on_open_series.clone();
                        let series_name = series_name.to_string();
                        move |_| on_open_series(series_name.clone())
                    });
                }
                duration_label.set_label(&format_duration(item.duration_seconds));
                cover.set_path(item.cover_cache_path.as_deref().map(std::path::Path::new));
                *cover_title.borrow_mut() = item.title.clone();
                if let Some(description) = item.description.as_deref().filter(|d| !d.is_empty()) {
                    // Descriptions are (sanitized) HTML server-side — Audiobookshelf's allowlist is
                    // `p, ol, ul, li, a, strong, em, del, br, b, i` — so render via Pango markup
                    // (`crate::rich_text` mirrors that allowlist and escapes everything else).
                    let markup = crate::rich_text::html_to_pango(description);
                    if !markup.is_empty() {
                        description_label.set_markup(&markup);
                        description_section.set_visible(true);
                        // A rough "does this need truncating" heuristic (no layout pass has run yet
                        // to know the real line count) — long enough that a short blurb never shows a
                        // pointless "more" button, short enough that a genuinely long description
                        // reliably gets one.
                        more_button.set_visible(description.chars().count() > 240);
                    }
                }
            }

            let progress = abs_storage::repo::progress::get(&pool, &account_id, &server_id, &item_id).await.ok().flatten();
            let progress_seconds = progress.as_ref().filter(|p| !p.is_finished).map(|p| p.current_time_seconds).unwrap_or(0.0);
            progress_seconds_cell.set(progress_seconds);
            let duration_seconds = item.as_ref().map(|i| i.duration_seconds).unwrap_or(0.0);
            duration_seconds_cell.set(duration_seconds);
            if progress_seconds > 0.0 {
                play_button.set_label(&tr("Resume"));
                if duration_seconds > 0.0 {
                    progress_bar.set_fraction((progress_seconds / duration_seconds).clamp(0.0, 1.0));
                    progress_bar.set_visible(true);
                }
            }
            // Its label is final now (a failed read above falls back to "Play"), so show it.
            play_button.set_opacity(1.0);
            play_button.set_sensitive(true);

            // Seeds the download menu from whatever chapters are already cached locally (a
            // previous play or download) — the same fallback Pass 2 (chapters) below falls back
            // to when the server can't be reached, just run eagerly here, still as part of Pass
            // 1's local-only work, so a book that's been played or downloaded before shows the
            // real per-chapter menu immediately rather than "Loading chapters…" for however long
            // the network resolve below takes. Pass 2 still runs regardless and overwrites these
            // same cells with the authoritative (possibly updated) chapter list once it resolves
            // — this is only ever a head start, never a final answer.
            let cached_chapters = abs_core::chapters::cached_chapters(&pool, &server_id, &item_id).await.unwrap_or_default();
            if !cached_chapters.is_empty() {
                let chapter_ranges: Vec<(f64, f64)> = cached_chapters.iter().map(|c| (c.start_seconds, c.end_seconds)).collect();
                current_chapter_index_cell.set(current_chapter_index_for(&chapter_ranges, progress_seconds));
                *chapter_ranges_cell.borrow_mut() = chapter_ranges;
                chapters_ready_cell.set(true);
                download_menu.refresh();
            }

            // Pass 2 (series): fetches the real "N/M" from the network, async and non-blocking —
            // `series_button` is already visible and clickable from Pass 1 above (plain name
            // only); this just updates its label text in place once the call resolves. A failure
            // here (offline, server hiccup, library not yet fetched) just leaves the Pass-1 label
            // as-is — never regresses a working local render because of a network error, same
            // posture the chapters fallback right below already has. See `abs_core::series`'s own
            // doc comment for why this call lives here rather than in the general sync cycle.
            if let Some(item) = &item {
                if item.series_name.as_deref().is_some_and(|s| !s.is_empty()) {
                    let library_id = item.library_id.clone();
                    let synced = match session.connection_target().await {
                        Ok(connection) => match connection.api_client_with_timeout(&session.access_token().await, std::time::Duration::from_secs(5)) {
                            Ok(api) => abs_core::series::sync_library_series(&pool, &api, &server_id, &library_id).await.is_ok(),
                            Err(err) => {
                                tracing::info!(%err, item_id = %item_id, "couldn't build an API client to refresh series info");
                                false
                            }
                        },
                        Err(err) => {
                            tracing::info!(%err, item_id = %item_id, "couldn't load connection settings to refresh series info");
                            false
                        }
                    };
                    if synced {
                        if let Ok(Some(info)) = abs_core::series::series_info_for_item(&pool, &server_id, &item_id).await {
                            let total_books = info.total_books.to_string();
                            let label = match &info.sequence {
                                // TRANSLATORS: {series} is a book series name, {seq} this book's position in it (e.g. "2" or "2.5"), {total} how many books the series has: "Dune, 2/6".
                                Some(seq) => tr_args("{series}, {seq}/{total}", &[("series", info.series_name.as_str()), ("seq", seq.as_str()), ("total", total_books.as_str())]),
                                // TRANSLATORS: {series} is a book series name, {count} how many books it has: "Dune (6 books)".
                                None => ntr_args(
                                    "{series} ({count} book)",
                                    "{series} ({count} books)",
                                    u32::try_from(info.total_books).unwrap_or(u32::MAX),
                                    &[("series", info.series_name.as_str())],
                                ),
                            };
                            series_button_label.set_label(&label);
                        }
                    }
                }
            }

            // Pass 2 (chapters + tracks): via the same `resolve_stream_target` call Player's own
            // `start()` makes. Falls back to whatever's already cached locally (a previous play
            // or download) if the server can't be reached, so the page still works offline —
            // same posture `PlayerController::start`'s own offline fallback already has.
            let chapters: Vec<(String, f64, f64)> = match session.connection_target().await {
                Ok(connection) => {
                    let access_token = session.access_token().await;
                    match abs_core::streaming::resolve_stream_target(&connection, &access_token, &item_id).await {
                        Ok(target) => {
                            if let Err(err) = abs_core::chapters::sync_item_chapters(&pool, &server_id, &item_id, &target.chapters).await {
                                tracing::warn!(%err, item_id = %item_id, "couldn't persist chapters locally");
                            }
                            target.chapters.iter().map(|c| (c.title.clone(), c.start_seconds, c.end_seconds)).collect()
                        }
                        Err(err) => {
                            tracing::info!(%err, item_id = %item_id, "couldn't resolve chapters from the server; using whatever is cached locally");
                            abs_core::chapters::cached_chapters(&pool, &server_id, &item_id)
                                .await
                                .unwrap_or_default()
                                .into_iter()
                                .map(|c| (c.title, c.start_seconds, c.end_seconds))
                                .collect()
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, item_id = %item_id, "couldn't load the server's connection settings; using whatever chapters are cached locally");
                    abs_core::chapters::cached_chapters(&pool, &server_id, &item_id)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|c| (c.title, c.start_seconds, c.end_seconds))
                        .collect()
                }
            };

            let chapter_ranges: Vec<(f64, f64)> = chapters.iter().map(|(_, start, end)| (*start, *end)).collect();
            let markers = abs_core::download_tracks::chapter_offline_markers_for_item(&pool, &server_id, &item_id, &chapter_ranges).await.unwrap_or_default();

            chapters_section.set_visible(!chapters.is_empty());
            *chapters_cell.borrow_mut() = chapters.clone();
            refresh_chapter_rows(&chapters_list, &chapters, progress_seconds, &markers, &on_play, &item_id);

            // Fills in the download menu's shared cells — the chapter the resume position falls
            // in, or 0 before it's known / for an unstarted book. Fixed once computed: unlike
            // Player (a live, ticking session), this page's position never changes under it, so
            // the closures `download_scope_menu::build` asks on every open just keep returning
            // the same values from here on. This is the authoritative chapter list (server-
            // resolved, or its own offline fallback) — it always wins over Pass 1's cache-seed
            // above, whether or not that one ran.
            let current_chapter_index = current_chapter_index_for(&chapter_ranges, progress_seconds);
            *chapter_ranges_cell.borrow_mut() = chapter_ranges;
            current_chapter_index_cell.set(current_chapter_index);
            chapters_ready_cell.set(true);
            download_menu.refresh();
        }
    });

    play_button.connect_clicked({
        let on_play = on_play.clone();
        let item_id = item_id.clone();
        let play_button = play_button.clone();
        move |_| {
            // `on_play` (`main_window::start_playback`) resolves asynchronously (network + stream
            // resolution, up to ~16s on a slow server) before it navigates away to the mini
            // bar/Full Player — without this, a tap here looked like it silently did nothing for
            // that whole window, and a second, impatient tap started a second concurrent resolve.
            // No explicit revert: `start_playback` always ends by opening the Full Player now
            // (`PlayerController::start` constructs `now_playing` — with `last_error` set — even
            // on failure), so this screen is swapped out by the time there's anything to show
            // again; a fresh `build()` next time this screen opens starts from "Play"/"Resume".
            play_button.set_sensitive(false);
            play_button.set_label(&tr("Starting…"));
            on_play(item_id.clone(), None);
        }
    });

    ItemDetailScreen {
        root: toast_overlay.clone().upcast(),
        #[cfg(test)]
        hooks: TestHooks {
            cover: cover.clone(),
            back_button,
            title_label,
            author_label,
            series_button,
            series_button_label,
            duration_label,
            progress_bar,
            play_button,
            description_label,
            more_button,
            chapters_list,
            download_button: download_menu.widget,
            download_popover: download_menu.popover,
            download_popover_box: download_menu.popover_box,
            #[cfg(test)]
            mark_as_finished_button: options_menu.mark_as_finished_button,
            #[cfg(test)]
            reset_progress_button: options_menu.reset_progress_button,
            mini_bar: mini_bar.hooks,
            download_progress_revealer: progress_strip_widget,
        },
    }
}

/// Which chapter `progress_seconds` (a book-level position) falls in, or `0` before it's known /
/// for an unstarted book — shared by the cache-seed fast path and Pass 2's own resolve below, so
/// the download menu's "current chapter" always means the same thing regardless of which of the
/// two filled it in.
fn current_chapter_index_for(chapter_ranges: &[(f64, f64)], progress_seconds: f64) -> usize {
    chapter_ranges.iter().position(|(start, end)| *start <= progress_seconds && progress_seconds < *end).unwrap_or(0)
}

/// Rebuilds `chapters_list`'s rows from scratch — the "currently playing" heading and the
/// "Downloaded" glyph both depend on state that can change after Pass 2 first builds this list
/// (a download completing while this screen is still open), so this is also called from a
/// `download_manager` listener, not just Pass 2 itself. `markers` is index-aligned with
/// `chapters`, same contract `abs_core::download_tracks::chapter_offline_markers_for_item`
/// already returns.
fn refresh_chapter_rows(
    chapters_list: &gtk4::ListBox,
    chapters: &[(String, f64, f64)],
    progress_seconds: f64,
    markers: &[bool],
    on_play: &Rc<dyn Fn(String, Option<usize>)>,
    item_id: &str,
) {
    while let Some(row) = chapters_list.row_at_index(0) {
        chapters_list.remove(&row);
    }
    for (index, (title, start, end)) in chapters.iter().enumerate() {
        let is_current = *start <= progress_seconds && progress_seconds < *end;
        let is_downloaded = markers.get(index).copied().unwrap_or(false);
        let row = adw::ActionRow::builder().title(title.as_str()).activatable(true).build();
        row.set_subtitle(&format_duration((end - start).max(0.0)));
        if is_current {
            row.add_css_class("heading");
        }
        if is_downloaded {
            row.add_suffix(&gtk4::Image::builder().icon_name("object-select-symbolic").css_classes(["dim-label"]).tooltip_text(tr("Downloaded")).build());
        }
        row.connect_activated({
            let on_play = on_play.clone();
            let item_id = item_id.to_string();
            move |_| {
                on_play(item_id.clone(), Some(index));
            }
        });
        chapters_list.append(&row);
    }
}

/// Formats a duration in seconds as a coarse, human-scale string — "1.5h" for anything an hour or
/// longer (chapters, whole books), "12m" below that (a very short chapter). Deliberately coarser
/// than the Player screen's `format_hms` (which needs second-level precision for a live scrubber);
/// this is metadata text, not a transport readout.
pub(crate) fn format_duration(total_seconds: f64) -> String {
    let total_seconds = total_seconds.max(0.0);
    if total_seconds >= 3600.0 {
        // TRANSLATORS: {hours} is a number of hours with one decimal, e.g. "1.5h" (a duration; "h" = hours).
        tr_args("{hours}h", &[("hours", &format!("{:.1}", total_seconds / 3600.0))])
    } else {
        // TRANSLATORS: {minutes} is a whole number of minutes, e.g. "12m" (a duration; "m" = minutes).
        tr_args("{minutes}m", &[("minutes", &((total_seconds / 60.0).round() as u64).to_string())])
    }
}

/// Everything "Mark as finished"/"Reset progress" need, for either write path (see the comment
/// above `item_options_menu::build`'s call in `build`).
#[derive(Clone)]
struct ProgressAction {
    controller: crate::player::PlayerController,
    pool: SqlitePool,
    session: abs_core::auth::Session,
    account_id: String,
    server_id: String,
    item_id: String,
    duration_seconds: Rc<Cell<f64>>,
    /// The unfinished position this screen shows (0 for an unstarted or finished book).
    progress_seconds: Rc<Cell<f64>>,
    play_button: gtk4::Button,
    progress_bar: gtk4::ProgressBar,
    toast_overlay: adw::ToastOverlay,
}

impl ProgressAction {
    fn is_loaded_in_player(&self) -> bool {
        self.controller.current_item_id().as_deref() == Some(self.item_id.as_str())
    }

    /// Shows `position_seconds` (0 or finished → "Play" with no bar) on this screen's own
    /// Play/Resume button and progress bar.
    fn show(&self, position_seconds: f64, is_finished: bool) {
        let unfinished_position = if is_finished { 0.0 } else { position_seconds };
        self.progress_seconds.set(unfinished_position);
        let duration = self.duration_seconds.get();
        if unfinished_position > 0.0 {
            self.play_button.set_label(&tr("Resume"));
            if duration > 0.0 {
                self.progress_bar.set_fraction((unfinished_position / duration).clamp(0.0, 1.0));
            }
            self.progress_bar.set_visible(duration > 0.0);
        } else {
            self.play_button.set_label(&tr("Play"));
            self.progress_bar.set_visible(false);
        }
    }

    /// Runs one of the menu's actions: `target` maps the book's duration to the
    /// `(position, is_finished)` to write. Both actions throw the listening position away, so the
    /// success toast offers Undo, which writes back what was there before.
    fn apply(&self, done_title: &str, error_context: &str, target: impl Fn(f64) -> (f64, bool)) {
        let (done_title, error_context) = (done_title.to_string(), error_context.to_string());
        let (position, is_finished) = target(self.duration_seconds.get());
        self.show(position, is_finished);
        if self.is_loaded_in_player() {
            // Still starting: the player's own position is 0 and the controller applies the
            // action once the book is loaded, so the Undo is built from what was saved.
            if self.controller.snapshot().is_some_and(|s| s.is_loading) {
                let action = self.clone();
                glib::spawn_future_local(async move {
                    let before = abs_storage::repo::progress::get(&action.pool, &action.account_id, &action.server_id, &action.item_id).await.ok().flatten();
                    let (before_position, before_finished) = before.map(|p| (p.current_time_seconds, p.is_finished)).unwrap_or((0.0, false));
                    if is_finished {
                        action.controller.mark_as_finished();
                    } else {
                        action.controller.reset_progress();
                    }
                    action.toast_overlay.add_toast(action.undo_toast(&done_title, before_position, before_finished));
                });
                return;
            }
            let before = self.controller.snapshot().map(|s| s.position_seconds).unwrap_or(0.0);
            // Synchronous from the caller's point of view (the controller's own write runs on
            // its queue, same posture as its periodic progress tick) — safe to toast right away.
            if is_finished {
                self.controller.mark_as_finished();
            } else {
                self.controller.reset_progress();
            }
            self.toast_overlay.add_toast(self.undo_toast(&done_title, before, false));
            return;
        }
        let action = self.clone();
        glib::spawn_future_local(async move {
            let before = abs_storage::repo::progress::get(&action.pool, &action.account_id, &action.server_id, &action.item_id).await.ok().flatten();
            // The local write is this action's real outcome — the toast reports exactly that,
            // not whether the best-effort server push also landed, which was never something
            // the toast promised.
            match action.write(position, is_finished).await {
                Ok(()) => {
                    let (before_position, before_finished) = before.map(|p| (p.current_time_seconds, p.is_finished)).unwrap_or((0.0, false));
                    action.toast_overlay.add_toast(action.undo_toast(&done_title, before_position, before_finished));
                }
                Err(err) => crate::error_reporting::report_background_error(&action.toast_overlay, &error_context, err),
            }
        });
    }

    async fn write(&self, position_seconds: f64, is_finished: bool) -> Result<(), abs_storage::StorageError> {
        write_item_progress(
            &self.pool,
            &self.session,
            ItemProgress {
                account_id: &self.account_id,
                server_id: &self.server_id,
                item_id: &self.item_id,
                duration_seconds: self.duration_seconds.get(),
            },
            position_seconds,
            is_finished,
        )
        .await
    }

    fn undo_toast(&self, title: &str, before_position: f64, before_finished: bool) -> adw::Toast {
        let toast = adw::Toast::builder().title(title).button_label(tr("Undo")).timeout(10).build();
        let action = self.clone();
        toast.connect_button_clicked(move |_| {
            action.show(before_position, before_finished);
            if action.is_loaded_in_player() {
                action.controller.seek_to_seconds(before_position);
                action.controller.save_progress_now();
                return;
            }
            let action = action.clone();
            glib::spawn_future_local(async move {
                if let Err(err) = action.write(before_position, before_finished).await {
                    crate::error_reporting::report_background_error(&action.toast_overlay, &tr("Undoing"), err);
                }
            });
        });
        toast
    }
}

/// Which item a direct progress write (one not going through the player) is for.
struct ItemProgress<'a> {
    account_id: &'a str,
    server_id: &'a str,
    item_id: &'a str,
    duration_seconds: f64,
}

/// Writes an item's progress locally, then pushes it to the server best-effort, clearing the
/// row's "needs push" mark once the server has it. Only the local write's result is returned: it
/// is what the action's toast reports. A failed push leaves the row marked, so the next sync or
/// reconnect pushes it.
async fn write_item_progress(
    pool: &sqlx::SqlitePool,
    session: &abs_core::auth::Session,
    item: ItemProgress<'_>,
    position_seconds: f64,
    is_finished: bool,
) -> Result<(), abs_storage::StorageError> {
    let ItemProgress { account_id, server_id, item_id, duration_seconds } = item;
    if let Err(err) = abs_storage::repo::progress::set(pool, account_id, server_id, item_id, position_seconds, is_finished).await {
        tracing::warn!(%err, item_id = %item_id, "couldn't write progress locally");
        return Err(err);
    }
    let connection = match session.connection_target().await {
        Ok(connection) => connection,
        Err(err) => {
            tracing::info!(%err, item_id = %item_id, "couldn't load connection settings; progress stays local until the next sync");
            return Ok(());
        }
    };
    let access_token = session.access_token().await;
    match abs_core::streaming::sync_progress_to_server(&connection, &access_token, item_id, position_seconds, duration_seconds, is_finished).await {
        Ok(()) => {
            if let Err(err) = abs_storage::repo::progress::mark_pushed(pool, account_id, server_id, item_id, position_seconds, is_finished).await {
                tracing::warn!(%err, item_id = %item_id, "couldn't record that progress reached the server; it will be pushed again");
            }
        }
        Err(err) => tracing::warn!(%err, item_id = %item_id, "couldn't push progress to the server; it stays marked for the next sync"),
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::{any_label_reads, pump_until};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_download_manager(pool: SqlitePool) -> DownloadManager {
        DownloadManager::new(pool, crate::test_support::test_paths(), Box::new(abs_player::network_watch::UnknownNetworkMonitor), false)
    }

    /// A `PlayerController` with nothing playing — enough for tests that don't exercise the mini
    /// bar's reflected state (that's covered by `run_shows_the_mini_bar_for_whatever_is_currently_
    /// playing` below, against a real, already-playing controller).
    fn test_controller(pool: SqlitePool) -> crate::player::PlayerController {
        crate::player::PlayerController::new(pool, crate::test_support::test_paths(), crate::player::tests::test_backend(), |_| {})
    }

    /// `on_play` calls recorded by the tests below — `(item_id, start_chapter)`.
    type PlayCalls = Rc<std::cell::RefCell<Vec<(String, Option<usize>)>>>;

    async fn account_and_server(pool: &SqlitePool, server_url: &str) -> (Server, Account) {
        let server_id = abs_storage::repo::servers::add(pool, server_url).await.unwrap();
        let account_id = abs_storage::repo::accounts::add(pool, &server_id, "jane", "token123", None).await.unwrap();
        (
            abs_storage::repo::servers::get(pool, &server_id).await.unwrap(),
            abs_storage::repo::accounts::get(pool, &account_id).await.unwrap(),
        )
    }

    /// Seeds the local `items`/`libraries` rows a card's tap always guarantees exist (Home/Library
    /// only ever make an item tappable once it's synced) — full metadata this time, since Item
    /// Detail actually renders narrator/description/duration, unlike other screens' thinner
    /// fixtures.
    #[allow(clippy::too_many_arguments)]
    async fn insert_synced_item(
        pool: &SqlitePool,
        server_id: &str,
        item_id: &str,
        title: &str,
        author: Option<&str>,
        narrator: Option<&str>,
        description: Option<&str>,
        duration_seconds: f64,
    ) {
        abs_storage::repo::libraries::upsert(
            pool,
            abs_storage::repo::libraries::UpsertLibrary { id: "lib-1", server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        abs_storage::repo::items::upsert(
            pool,
            abs_storage::repo::items::UpsertItem { id: item_id, server_id, library_id: "lib-1", title, author, narrator, description, duration_seconds, added_at: chrono::Utc::now(), series_name: None, genres: &[] },
        )
        .await
        .unwrap();
    }

    /// Like [`insert_synced_item`], but with a `series_name` set — the series tests below don't
    /// need narrator/description, just a title and a series to look up.
    async fn insert_synced_item_with_series(pool: &SqlitePool, server_id: &str, item_id: &str, title: &str, series_name: &str, duration_seconds: f64) {
        abs_storage::repo::libraries::upsert(
            pool,
            abs_storage::repo::libraries::UpsertLibrary { id: "lib-1", server_id, name: "Audiobooks", media_type: "book", icon: None, display_order: 1 },
        )
        .await
        .unwrap();
        abs_storage::repo::items::upsert(
            pool,
            abs_storage::repo::items::UpsertItem {
                id: item_id,
                server_id,
                library_id: "lib-1",
                title,
                author: None,
                narrator: None,
                description: None,
                duration_seconds,
                added_at: chrono::Utc::now(),
                series_name: Some(series_name),
                genres: &[],
            },
        )
        .await
        .unwrap();
    }

    async fn mock_item_with_chapters(mock_server: &MockServer, item_id: &str, seconds: f64, chapters: &[(&str, f64, f64)]) {
        let chapters_json: Vec<_> = chapters.iter().enumerate().map(|(i, (title, start, end))| serde_json::json!({ "id": i, "start": start, "end": end, "title": title })).collect();
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [{ "ino": "1", "duration": seconds }], "chapters": chapters_json }
            })))
            .mount(mock_server)
            .await;
    }

    /// Like [`mock_item_with_chapters`], but the item endpoint (Pass 2's own `resolve_stream_target`
    /// call) is held for `delay` before responding — for tests that need a real window in which
    /// the download menu can be opened *before* chapters have resolved.
    async fn mock_item_with_chapters_delayed(mock_server: &MockServer, item_id: &str, seconds: f64, chapters: &[(&str, f64, f64)], delay: Duration) {
        let chapters_json: Vec<_> = chapters.iter().enumerate().map(|(i, (title, start, end))| serde_json::json!({ "id": i, "start": start, "end": end, "title": title })).collect();
        Mock::given(method("GET"))
            .and(path(format!("/api/items/{item_id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "media": { "audioFiles": [{ "ino": "1", "duration": seconds }], "chapters": chapters_json }
                    }))
                    .set_delay(delay),
            )
            .mount(mock_server)
            .await;
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Metadata renders from the local cache
    /// synchronously (before the chapters network call even lands): title, author + narrator,
    /// duration, and no progress bar / a "Play" label for a never-started book (ID-1, ID-3).
    pub(crate) fn run_metadata_renders_from_cache_and_play_for_unstarted(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Project Hail Mary", Some("Andy Weir"), Some("Ray Porter"), Some("A lone astronaut."), 3600.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.title_label.label() == "Project Hail Mary", Duration::from_secs(5));
        assert_eq!(hooks.author_label.label(), "Andy Weir · Narrated by Ray Porter");
        assert_eq!(hooks.duration_label.label(), "1.0h");
        // The title is filled in before the progress read; the button only once that's done.
        pump_until(|| hooks.play_button.opacity() == 1.0, Duration::from_secs(5));
        assert_eq!(hooks.play_button.label().as_deref(), Some("Play"), "an unstarted book's primary button should read Play");
        assert!(hooks.play_button.opacity() == 1.0 && hooks.play_button.is_sensitive(), "the button is shown once its label is known");
        assert!(!hooks.progress_bar.is_visible(), "no progress bar for an unstarted book");

        // The title can be selected and copied, but never takes focus — so opening the page
        // doesn't highlight it, the way GTK does with the first focusable label it finds.
        assert!(hooks.title_label.is_selectable(), "the title can be selected for copying");
        assert!(!hooks.title_label.is_focusable(), "the title must not be focusable");
        let window = gtk4::Window::builder().child(&screen.root).default_width(360).default_height(720).build();
        window.present();
        pump_until(|| false, Duration::from_millis(300));
        assert!(!hooks.title_label.is_focusable(), "showing the page must not make the title focusable again");
        assert_eq!(hooks.title_label.selection_bounds(), None, "nothing is pre-selected when the page opens");
        assert!(!hooks.title_label.has_focus(), "focus never lands on the title");
        window.close();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A server that sends an empty-string
    /// narrator (distinct from the field being absent entirely, which
    /// `run_partially_listened_book_shows_resume_and_progress` already covers) must fall back to
    /// the author-only subtitle rather than rendering a dangling "Narrated by" with nothing after
    /// it.
    pub(crate) fn run_empty_narrator_falls_back_to_author_only(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Meditations", Some("Marcus Aurelius"), Some(""), None, 3600.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.title_label.label() == "Meditations", Duration::from_secs(5));
        assert_eq!(hooks.author_label.label(), "Marcus Aurelius", "an empty narrator string must not print a dangling 'Narrated by'");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A partially-listened book's primary
    /// button reads "Resume" and the progress bar reflects the stored fraction (ID-3).
    pub(crate) fn run_partially_listened_book_shows_resume_and_progress(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Project Hail Mary", Some("Andy Weir"), None, None, 3600.0));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 1800.0, false)).unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();
        // Before the progress has been read the button must not show (or take) a "Play" that
        // then turns into "Resume".
        assert!(
            hooks.play_button.opacity() == 0.0 && !hooks.play_button.is_sensitive(),
            "the button stays hidden until its label is known, showing {:?}",
            hooks.play_button.label()
        );

        pump_until(|| hooks.play_button.label().as_deref() == Some("Resume"), Duration::from_secs(5));
        assert!(hooks.play_button.opacity() == 1.0 && hooks.play_button.is_sensitive(), "and is shown, reading Resume, once it is");
        assert!(hooks.progress_bar.is_visible());
        assert!((hooks.progress_bar.fraction() - 0.5).abs() < 0.01, "1800s of 3600s should be a 50% progress bar");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The detail page's cover opens the
    /// viewer too. Offline, with no original fetched yet, it shows the small copy and says so;
    /// once an original is in the cache it's shown offline as well. A book without a cover
    /// isn't tappable.
    pub(crate) fn run_the_cover_opens_the_viewer_offline(runtime: &tokio::runtime::Runtime) {
        use crate::widgets::cover_viewer::{self, tests::*, Pill};
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-2", 3600.0, &[]));
        runtime.block_on(mount_original(&mock_server, "item-1", Duration::ZERO, 0));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Project Hail Mary", Some("Andy Weir"), None, None, 3600.0));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-2", "No Cover Here", None, None, None, 3600.0));
        let controller = test_controller(pool.clone());
        let paths = controller.paths();
        runtime.block_on(seed_thumbnail(&pool, &paths, &server.id, "item-1"));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        session.set_offline(true);
        let screen = build(pool.clone(), server.clone(), account.clone(), session.clone(), test_download_manager(pool.clone()), controller.clone(), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();
        let window = adw::ApplicationWindow::builder().default_width(360).default_height(648).build();
        window.set_content(Some(&screen.root));
        window.present();
        pump_until(|| window.is_mapped() && hooks.cover.picture().is_visible(), Duration::from_secs(5));

        hooks.cover.tap();
        let viewer = wait_until_shown(Duration::from_secs(2));
        wait_for_texture(&viewer, THUMBNAIL_SIZE, Duration::from_secs(5));
        pump_until(|| matches!(viewer.pill(), Pill::Note(_)), Duration::from_secs(2));
        assert_eq!(viewer.pill(), Pill::Note(cover_viewer::SMALLER_COPY_NOTE.to_string()));
        close_button(&viewer).emit_clicked();
        pump_until(|| false, Duration::from_millis(200));
        assert_eq!(content_of(window.upcast_ref()).as_ref(), Some(&screen.root), "the detail page is back");

        // An original fetched on an earlier open is shown offline.
        let original = paths.original_cover_path(&server.id, "item-1", "png");
        std::fs::write(&original, png(&cover_art(ORIGINAL_SIZE.0, ORIGINAL_SIZE.1))).unwrap();
        hooks.cover.tap();
        let viewer = wait_until_shown(Duration::from_secs(2));
        wait_for_texture(&viewer, ORIGINAL_SIZE, Duration::from_secs(20));
        pump_until(|| viewer.pill() == Pill::Hidden, Duration::from_secs(2));
        assert_eq!(viewer.pill(), Pill::Hidden);
        viewer.close();
        pump_until(|| false, Duration::from_millis(200));

        // No cover: nothing to open.
        let coverless = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), controller, "item-2".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        window.set_content(Some(&coverless.root));
        pump_until(|| coverless.test_hooks().title_label.label() == "No Cover Here", Duration::from_secs(5));
        coverless.test_hooks().cover.tap();
        pump_until(|| false, Duration::from_millis(300));
        assert!(cover_viewer::current().is_none(), "a book without a cover opens no viewer");
        assert_eq!(content_of(window.upcast_ref()).as_ref(), Some(&coverless.root));
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The actions row's two buttons must
    /// render at the same height: the spacing above the row lives on the row itself, so the
    /// download `MenuButton` (a default Fill-valign child, like every widget) can't be stretched
    /// taller than the pill beside it by a one-sided child margin — the on-device bug this guards.
    /// Needs a mapped window: heights are only meaningful once the row has been allocated.
    pub(crate) fn run_action_buttons_render_equal_heights(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Project Hail Mary", Some("Andy Weir"), None, None, 3600.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));
        // Both buttons need their own allocation pass before comparing — the download button's
        // icon can lag a frame behind the pill's label (icon-theme lookup vs. plain text layout),
        // so waiting on the pill alone flakes with a spurious `0` for the download button.
        pump_until(|| hooks.play_button.allocated_height() > 0 && hooks.download_button.allocated_height() > 0, Duration::from_secs(5));

        assert_eq!(
            hooks.play_button.allocated_height(),
            hooks.download_button.allocated_height(),
            "the Play/Resume and Download buttons must render at the same height"
        );
        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A long description truncates to a few
    /// lines with a "more" toggle that expands it to the full text (ID-4); a short description
    /// shows no toggle at all.
    pub(crate) fn run_description_truncation_and_more_toggle(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let long_description = "A".repeat(400);
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, Some(&long_description), 3600.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.description_label.label() == long_description, Duration::from_secs(5));
        assert!(hooks.more_button.is_visible(), "a long description should show the 'more' toggle");
        assert_eq!(hooks.description_label.lines(), 3, "should start truncated to a few lines");

        hooks.more_button.emit_clicked();
        assert_eq!(hooks.description_label.lines(), -1, "tapping 'more' should expand to the full text");
        assert_eq!(hooks.more_button.label().as_deref(), Some("less"));
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Descriptions are (sanitized) HTML
    /// server-side, so the label renders via Pango markup: paragraphs break, `&amp;` decodes —
    /// and malformed HTML (unclosed `<b>`/`<i>` here) still renders every word of text.
    pub(crate) fn run_description_renders_html_as_markup(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        let description = "<p>First para</p><p>Second <b>bold &amp; <i>unclosed";
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, Some(description), 3600.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.description_label.uses_markup(), Duration::from_secs(5));
        assert_eq!(hooks.description_label.label(), "First para\n\nSecond <b>bold &amp; <i>unclosed</i></b>");
        assert_eq!(hooks.description_label.text(), "First para\n\nSecond bold & unclosed");
        assert!(!hooks.more_button.is_visible(), "a short description should show no 'more' toggle");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Chapter rows list title + duration,
    /// show the offline glyph only for chapters whose tracks are fully downloaded (ID-5, ID-15),
    /// and tapping a row starts playback at that chapter — navigation is the caller's job now
    /// (`on_play` opens the full Player screen itself), so this screen never calls `on_back` here.
    pub(crate) fn run_chapter_rows_show_offline_glyphs_and_seek_on_tap(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 10.0, &[("Intro", 0.0, 4.0), ("Chapter One", 4.0, 10.0)]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 10.0));
        // "Intro"'s only track (the book's single audio file) is fully downloaded; there is no
        // second file, so this is the straddling-a-single-track case: both chapters map onto the
        // same one track, so a completed download makes *both* chapters show the glyph — the
        // point of this assertion is that a chapter with *no* completed backing track (tested
        // below, on a two-track item) shows none at all.
        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-1", &[abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 10.0, offset_seconds: 0.0, size_bytes: None }])).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::upsert_pending(&pool, &server.id, "item-1", "1", "/p/1.mp3")).unwrap();
        runtime.block_on(abs_storage::repo::download_tracks::mark_complete(&pool, &server.id, "item-1", "1", 10)).unwrap();

        let played: PlayCalls = Rc::new(std::cell::RefCell::new(Vec::new()));
        let went_back = Rc::new(Cell::new(false));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), {
            let played = played.clone();
            move |item_id: String, chapter: Option<usize>| played.borrow_mut().push((item_id, chapter))
        }, {
            let went_back = went_back.clone();
            move || went_back.set(true)
        }, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.chapters_list.row_at_index(1).is_some(), Duration::from_secs(5));
        let row0 = hooks.chapters_list.row_at_index(0).unwrap().downcast::<adw::ActionRow>().unwrap();
        let row1 = hooks.chapters_list.row_at_index(1).unwrap().downcast::<adw::ActionRow>().unwrap();
        assert_eq!(row0.title(), "Intro");
        assert_eq!(row1.title(), "Chapter One");
        assert!(row0.child().is_some());

        row1.emit_by_name::<()>("activated", &[]);
        assert_eq!(*played.borrow(), vec![("item-1".to_string(), Some(1))], "tapping a chapter row should start playback at that chapter");
        assert!(!went_back.get(), "tapping a chapter row should leave navigation to the caller's on_play, not call on_back itself");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Tapping Play calls through to
    /// `on_play` with no chapter override — the standard Play/Resume path (ID-3). The caller's
    /// `on_play` is expected to take over navigation itself (opening the full Player screen), so
    /// this screen never calls `on_back` here.
    pub(crate) fn run_tapping_play_invokes_on_play_and_leaves_navigation_to_the_caller(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 3600.0));

        let played: PlayCalls = Rc::new(std::cell::RefCell::new(Vec::new()));
        let went_back = Rc::new(Cell::new(false));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), {
            let played = played.clone();
            move |item_id: String, chapter: Option<usize>| played.borrow_mut().push((item_id, chapter))
        }, {
            let went_back = went_back.clone();
            move || went_back.set(true)
        }, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.title_label.label() == "Test Item", Duration::from_secs(5));
        hooks.play_button.emit_clicked();

        assert_eq!(*played.borrow(), vec![("item-1".to_string(), None)]);
        assert!(!went_back.get(), "tapping Play should leave navigation to the caller's on_play, not call on_back itself");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The back button calls `on_back`.
    pub(crate) fn run_back_button_invokes_on_back(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 3600.0));

        let went_back = Rc::new(Cell::new(false));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, {
            let went_back = went_back.clone();
            move || went_back.set(true)
        }, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        hooks.back_button.emit_clicked();
        assert!(went_back.get());
    }

    /// How many progress PATCHes `mock_server` has received so far.
    fn progress_patches(runtime: &tokio::runtime::Runtime, mock_server: &MockServer) -> usize {
        runtime
            .block_on(mock_server.received_requests())
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "PATCH" && request.url.path().starts_with("/api/me/progress/"))
            .count()
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. "Mark as finished" on an item that
    /// isn't currently loaded into `controller` must still take effect: a direct local write plus
    /// a best-effort push to the server, and the screen's own Play button/progress bar reflecting
    /// it immediately.
    pub(crate) fn run_options_menu_marks_finished_via_direct_write_when_not_currently_playing(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));
        runtime.block_on(async {
            Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server).await;
        });

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 3600.0));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 1800.0, false)).unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server.clone(), account.clone(), session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();
        pump_until(|| hooks.play_button.label().as_deref() == Some("Resume"), Duration::from_secs(5));

        let requests_before = progress_patches(runtime, &mock_server);
        hooks.mark_as_finished_button.emit_clicked();

        assert_eq!(hooks.play_button.label().as_deref(), Some("Play"), "the button should reflect the new state immediately, not wait on the async write");
        assert!(!hooks.progress_bar.is_visible());

        pump_until(
            || runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap().is_finished,
            Duration::from_secs(5),
        );
        let progress = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert_eq!(progress.current_time_seconds, 3600.0, "should be recorded at the item's full duration");

        // The local write (polled above) and the server push are two separate awaits in
        // sequence, not one atomic step — the local write landing is not proof the push has too.
        // Counts only the progress PATCH: the screen's own background item fetch can land in
        // this window too, and counting every request made this flaky.
        pump_until(|| progress_patches(runtime, &mock_server) - requests_before == 1, Duration::from_secs(5));
        assert_eq!(progress_patches(runtime, &mock_server) - requests_before, 1, "the direct write should also push exactly one PATCH to the server");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Same shape as the mark-as-finished
    /// test above, for "Reset progress".
    pub(crate) fn run_options_menu_resets_progress_via_direct_write_when_not_currently_playing(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));
        runtime.block_on(async {
            Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server).await;
        });

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 3600.0));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 1800.0, false)).unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server.clone(), account.clone(), session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();
        pump_until(|| hooks.play_button.label().as_deref() == Some("Resume"), Duration::from_secs(5));

        let requests_before = progress_patches(runtime, &mock_server);
        hooks.reset_progress_button.emit_clicked();

        assert_eq!(hooks.play_button.label().as_deref(), Some("Play"));
        assert!(!hooks.progress_bar.is_visible());

        pump_until(
            || runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap().current_time_seconds == 0.0,
            Duration::from_secs(5),
        );
        let progress = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert!(!progress.is_finished);

        // The local write (polled above) and the server push are two separate awaits in
        // sequence, not one atomic step — the local write landing is not proof the push has too.
        // Counts only the progress PATCH: the screen's own background item fetch can land in
        // this window too, and counting every request made this flaky.
        pump_until(|| progress_patches(runtime, &mock_server) - requests_before == 1, Duration::from_secs(5));
        assert_eq!(progress_patches(runtime, &mock_server) - requests_before, 1, "the direct write should also push exactly one PATCH to the server");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. "Reset progress" throws the
    /// listening position away in one tap, so its toast offers Undo, which must put the old
    /// position back locally, on the server, and on this screen.
    pub(crate) fn run_options_menu_reset_can_be_undone(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));
        runtime.block_on(async {
            Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server).await;
        });

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 3600.0));
        runtime.block_on(abs_storage::repo::progress::set(&pool, &account.id, &server.id, "item-1", 1800.0, false)).unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server.clone(), account.clone(), session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();
        pump_until(|| hooks.play_button.label().as_deref() == Some("Resume"), Duration::from_secs(5));

        hooks.reset_progress_button.emit_clicked();
        let saved = || runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        pump_until(|| crate::test_support::find_button_with_label(&screen.root, "Undo").is_some(), Duration::from_secs(5));
        assert_eq!(saved().current_time_seconds, 0.0);

        crate::test_support::find_button_with_label(&screen.root, "Undo").expect("the reset toast should offer Undo").emit_clicked();
        assert_eq!(hooks.play_button.label().as_deref(), Some("Resume"), "Undo should restore the screen right away");
        assert!(hooks.progress_bar.is_visible());
        pump_until(|| saved().current_time_seconds == 1800.0, Duration::from_secs(5));
        assert_eq!(saved().current_time_seconds, 1800.0, "Undo must write the old position back");
        pump_until(|| !saved().needs_push, Duration::from_secs(5));
        let last_patch: serde_json::Value = runtime
            .block_on(mock_server.received_requests())
            .unwrap()
            .iter()
            .rev()
            .find(|r| r.method.as_str() == "PATCH")
            .unwrap()
            .body_json()
            .unwrap();
        assert_eq!(last_patch["currentTime"], 1800.0, "and push it back to the server");
    }

    /// Regression test for the "the toast said it worked, but the write never landed" gap:
    /// before this, "Mark as finished" toasted unconditionally, synchronously, whether or not the
    /// direct write actually succeeded. Forces a real failure (the item was never synced locally,
    /// so `progress`'s `FOREIGN KEY (server_id, item_id) REFERENCES items` rejects the insert) and
    /// asserts the failure toast, not "Marked as finished", is what actually shows.
    pub(crate) fn run_options_menu_direct_write_failure_toasts_the_failure(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(async {
            Mock::given(method("PATCH")).and(path("/api/me/progress/item-1")).respond_with(ResponseTemplate::new(200)).mount(&mock_server).await;
        });

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        // Deliberately no `insert_synced_item` — "item-1" doesn't exist in the local `items`
        // table, so `progress`'s FK constraint makes the direct write fail for real.

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server.clone(), account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        hooks.mark_as_finished_button.emit_clicked();

        pump_until(|| any_label_reads(&screen.root, "Marking as finished failed — try again"), Duration::from_secs(5));
        assert!(
            any_label_reads(&screen.root, "Marking as finished failed — try again"),
            "a real write failure must toast the failure, not claim success"
        );
        assert!(!any_label_reads(&screen.root, "Marked as finished"), "the success toast must not also appear");

        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. When this item *is* the one currently
    /// loaded into `controller`, "Mark as finished" must go through the live controller (which
    /// also pauses real playback) rather than the direct-write path above.
    pub(crate) fn run_options_menu_delegates_to_the_live_controller_when_this_item_is_playing(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 5.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let controller = test_controller(pool.clone());
        controller.start(session.clone(), crate::player::PlayRequest { item_id: "item-1".to_string(), title: "Test Item".to_string(), author: None }, 1.0);
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        let screen = build(pool.clone(), server.clone(), account.clone(), session, test_download_manager(pool.clone()), controller.clone(), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        hooks.mark_as_finished_button.emit_clicked();
        pump_until(|| !controller.snapshot().unwrap().is_playing, Duration::from_secs(5));
        assert!(!controller.snapshot().unwrap().is_playing, "marking finished should pause the live controller, proving the live path was taken");

        pump_until(
            || runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().is_some_and(|p| p.is_finished),
            Duration::from_secs(5),
        );
        let progress = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert_eq!(progress.current_time_seconds, 5.0, "should be recorded at the item's full duration, via the live controller's own write path");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The embedded `DownloadScopeMenu`
    /// reaches a real download when a scope row is tapped — thin coverage, since the menu's own
    /// stepper/estimate/free-space behavior is fully covered by `widgets::download_scope_menu`'s
    /// own tests.
    pub(crate) fn run_download_menu_starts_a_real_download(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(crate::downloads::tests::mock_two_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 10.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server.clone(), account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.download_button.is_sensitive(), Duration::from_secs(5));

        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        hooks.download_popover.popup();
        // Waits for the real "Entire book" row specifically, not just any child — the popover's
        // first child while chapters haven't resolved yet is a "Loading chapters…" placeholder
        // with no scope row at all (see `download_scope_menu::build`'s own doc), which
        // `first_child().is_some()` alone can't distinguish from the real rows.
        pump_until(|| any_label_reads(hooks.download_popover_box.upcast_ref(), "Entire book"), Duration::from_secs(5));
        click_button_labeled(&hooks.download_popover_box, "Entire book");

        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );
        pump_until(|| hooks.download_button.icon_name().as_deref() == Some("object-select-symbolic"), Duration::from_secs(5));
        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Regression test for the "no visible
    /// progress, and the chapter glyphs go stale" gaps: starting a whole-book download from this
    /// screen must reveal the progress strip immediately, and once the batch completes, both the
    /// strip must hide again *and* every chapter row must pick up its "Downloaded" glyph without
    /// the screen being rebuilt — `download_manager::tests::mock_three_track_item` gives real
    /// per-chapter tracks to download against.
    pub(crate) fn run_download_progress_strip_reveals_and_chapter_glyphs_update_live(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(crate::downloads::tests::mock_three_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 15.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server.clone(), account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        // Pass 2 (chapters over the network) must have landed before any row exists to assert on.
        pump_until(|| hooks.chapters_list.row_at_index(0).is_some(), Duration::from_secs(5));
        assert!(!hooks.download_progress_revealer.reveals_child(), "nothing is downloading yet");

        hooks.download_popover.popup();
        pump_until(|| hooks.download_popover_box.first_child().is_some(), Duration::from_secs(2));
        click_button_labeled(&hooks.download_popover_box, "Entire book");

        pump_until(|| hooks.download_progress_revealer.reveals_child(), Duration::from_secs(5));
        assert!(hooks.download_progress_revealer.reveals_child(), "starting a download should reveal the progress strip immediately");

        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::downloaded_item_ids(&pool, &server.id)).unwrap().iter().any(|id| id == "item-1"),
            Duration::from_secs(10),
        );
        pump_until(|| !hooks.download_progress_revealer.reveals_child(), Duration::from_secs(5));
        assert!(!hooks.download_progress_revealer.reveals_child(), "the strip should retract once the batch completes");

        let all_three_chapter_rows_marked = || {
            (0..3).all(|i| hooks.chapters_list.row_at_index(i).is_some_and(|row| find_descendant_in_row::<gtk4::Image>(&row).is_some()))
        };
        pump_until(all_three_chapter_rows_marked, Duration::from_secs(5));
        assert!(all_three_chapter_rows_marked(), "every chapter should show the Downloaded glyph without reopening the screen");
        assert!(hooks.chapters_list.row_at_index(3).is_none(), "the item has exactly 3 chapters");

        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Regression test for the reported bug:
    /// tapping Download right after opening a book, before its chapters have resolved from the
    /// network, used to show every scope at the whole-book size with "Current chapter" still
    /// tappable into a full-book download. The item endpoint is held open for 500ms so there's a
    /// real window to open the menu in before Pass 2 (chapters) can possibly have landed.
    pub(crate) fn run_download_menu_shows_a_loading_placeholder_until_chapters_resolve(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters_delayed(
            &mock_server,
            "item-1",
            15.0,
            &[("One", 0.0, 5.0), ("Two", 5.0, 10.0), ("Three", 10.0, 15.0)],
            Duration::from_millis(500),
        ));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 15.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        // Open the menu right away — Pass 2's 500ms delay guarantees chapters can't have landed.
        hooks.download_popover.popup();
        pump_until(|| hooks.download_popover_box.first_child().is_some(), Duration::from_secs(2));

        assert!(
            any_label_reads(hooks.download_popover_box.upcast_ref(), "Loading chapters…"),
            "opening the menu before chapters resolve must show the loading placeholder"
        );
        assert!(
            !any_label_reads(hooks.download_popover_box.upcast_ref(), "Current chapter"),
            "no scope row must exist while chapters aren't ready — nothing should be tappable into a whole-book download"
        );

        // Let Pass 2 land while the popover is still open — it must update in place.
        pump_until(|| any_label_reads(hooks.download_popover_box.upcast_ref(), "Current chapter"), Duration::from_secs(5));
        assert!(
            !any_label_reads(hooks.download_popover_box.upcast_ref(), "Loading chapters…"),
            "the loading placeholder should be gone once chapters land, with no reopen needed"
        );

        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A book that's been played or
    /// downloaded before already has its chapters cached locally — the download menu should show
    /// the real per-chapter options immediately from that cache, with no wait for Pass 2's network
    /// resolve at all. The item endpoint is delayed generously so any real rows the test observes
    /// can only have come from the cache seed, not from Pass 2 landing early.
    pub(crate) fn run_download_menu_uses_cached_chapters_immediately(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters_delayed(
            &mock_server,
            "item-1",
            15.0,
            &[("One", 0.0, 5.0), ("Two", 5.0, 10.0), ("Three", 10.0, 15.0)],
            Duration::from_secs(3),
        ));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 15.0));
        // Seeds exactly what a previous play/download would have left behind — same local DB
        // rows `abs_core::chapters::cached_chapters` (the cache-seed path's own read) resolves.
        runtime.block_on(abs_core::chapters::sync_item_chapters(
            &pool,
            &server.id,
            "item-1",
            &[
                abs_api::ChapterRef { title: "One".to_string(), start_seconds: 0.0, end_seconds: 5.0 },
                abs_api::ChapterRef { title: "Two".to_string(), start_seconds: 5.0, end_seconds: 10.0 },
                abs_api::ChapterRef { title: "Three".to_string(), start_seconds: 10.0, end_seconds: 15.0 },
            ],
        ))
        .unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        hooks.download_popover.popup();
        pump_until(|| any_label_reads(hooks.download_popover_box.upcast_ref(), "Current chapter"), Duration::from_secs(2));

        assert!(
            any_label_reads(hooks.download_popover_box.upcast_ref(), "Current chapter"),
            "cached chapters should populate the real menu immediately, well before Pass 2's 3s delay elapses"
        );
        assert!(!any_label_reads(hooks.download_popover_box.upcast_ref(), "Loading chapters…"));

        window.destroy();
    }

    /// Depth-first search for a widget of type `T` under a chapter row — same purpose as
    /// `widgets::find_descendant`, duplicated here rather than made `pub(crate)` there since this
    /// crosses from a `gtk4::ListBoxRow` (this screen's chapter rows) rather than the plain
    /// `GtkWidget` root every other call site searches from.
    fn find_descendant_in_row<T: glib::object::IsA<gtk4::Widget>>(row: &gtk4::ListBoxRow) -> Option<T> {
        crate::widgets::find_descendant::<T>(row.upcast_ref())
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The direct regression test for the
    /// reported bug: Item Detail's own mini bar must reflect the exact same live
    /// `PlayerController` the shell's mini bar shows — not a second, independent copy of it — and
    /// tapping it should open the full player.
    pub(crate) fn run_shows_the_mini_bar_for_whatever_is_currently_playing(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item", None, None, None, 5.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let controller = test_controller(pool.clone());
        controller.start(
            session.clone(),
            crate::player::PlayRequest { item_id: "item-1".to_string(), title: "Test Item".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));

        let opened_player = Rc::new(Cell::new(false));
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), controller.clone(), "item-1".to_string(), |_, _| {}, || {}, {
            let opened_player = opened_player.clone();
            move || opened_player.set(true)
        }, |_| {}, || {});
        let hooks = screen.test_hooks();

        // The mini bar is primed immediately from `controller.snapshot()` at build time — no need
        // to wait for the next tick to see it reflect what's already playing.
        assert!(hooks.mini_bar.bar.is_visible(), "the mini bar should show once something is playing");
        assert!(hooks.mini_bar.bar.has_css_class("mini-player"), "the bar sits on the tab-bar-styled strip");
        assert!(hooks.mini_bar.card.has_css_class("card"), "the bar itself is drawn as a card");
        assert_eq!(hooks.mini_bar.title_label.label(), "Test Item");

        hooks.mini_bar.play_button.emit_clicked();
        pump_until(|| !controller.snapshot().unwrap().is_playing, Duration::from_secs(5));
        assert!(!controller.snapshot().unwrap().is_playing, "the mini bar's play button should control the exact same shared controller");

        // No new assertion for the tap/swipe-to-open gesture itself: `GestureDrag::offset()`
        // depends on the controller's own tracked pointer state from a real press/motion/release,
        // which nothing in this codebase can synthesize (the same limitation `main_window`'s own
        // mini-bar swipe-up gesture tests hit) — its decision logic
        // (`mini_bar_gesture_should_open`) is already unit-tested there, and the wiring here is
        // the same one-line `if` calling an already-tested callback.
        controller.stop();
    }

    /// The series button (ID: series info + tap-through) — Pass 1 shows the plain name
    /// immediately from the local item row; Pass 2 fills in the real "N/M" once the network call
    /// resolves, updating the *same* widget's text in place, never swapping it or letting it wrap
    /// to a second line (the `max_width_chars`/`ellipsize` cap is what actually prevents the
    /// "lower part of the screen jumps" regression this test guards against). The button is
    /// already clickable during the brief window before Pass 2 lands.
    pub(crate) fn run_series_button_shows_the_plain_name_then_fills_in_the_real_numbers(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));
        runtime.block_on(async {
            Mock::given(method("GET"))
                .and(path("/api/libraries/lib-1/series"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [{
                        "id": "series-1",
                        "name": "Foundation",
                        "books": [
                            { "id": "item-1", "sequence": "2" },
                            { "id": "item-2", "sequence": "1" },
                            { "id": "item-3", "sequence": "3" }
                        ]
                    }]
                })))
                .mount(&mock_server)
                .await;
        });

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item_with_series(&pool, &server.id, "item-1", "Foundation", "Foundation", 3600.0));

        let opened_series: Rc<std::cell::RefCell<Vec<String>>> = Rc::new(std::cell::RefCell::new(Vec::new()));
        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, {
            let opened_series = opened_series.clone();
            move |name: String| opened_series.borrow_mut().push(name)
        }, || {});
        let hooks = screen.test_hooks();

        // Pass 1 lands: title and the series button's plain name are set in the same
        // uninterrupted stretch of async code (no `.await` between them), so once the title is
        // there, the series button is guaranteed to be too — proof it didn't wait on Pass 2's
        // network call.
        pump_until(|| hooks.title_label.label() == "Foundation", Duration::from_secs(5));
        assert!(hooks.series_button.is_visible(), "the series button should appear as soon as Pass 1's local read lands");
        assert_eq!(hooks.series_button_label.label(), "Foundation", "Pass 1 shows just the plain name, no numbers yet");

        // Clickable right away, before Pass 2 has had any chance to resolve.
        hooks.series_button.emit_clicked();
        assert_eq!(*opened_series.borrow(), vec!["Foundation".to_string()], "tapping the button before Pass 2 lands should still open the series by name");

        // Pass 2 lands: the same widget's text updates in place to include the real numbers.
        pump_until(|| hooks.series_button_label.label() == "Foundation, 2/3", Duration::from_secs(5));
        assert!(hooks.series_button.is_visible(), "the button must not be hidden/rebuilt when Pass 2 updates its text");
        // The label is capped to one line regardless of which pass set its text — this is the
        // actual mechanism that prevents the reported "lower part of the screen jumps" bug: a
        // capped, non-wrapping label can only truncate/extend within a fixed-height row.
        assert_eq!(hooks.series_button_label.max_width_chars(), 1);
        assert_eq!(hooks.series_button_label.ellipsize(), gtk4::pango::EllipsizeMode::End);
        assert!(!hooks.series_button_label.wraps(), "the series label must never wrap to a second line");
    }

    /// A network failure (or the library's series list simply not existing yet) must leave the
    /// Pass-1 plain-name label exactly as it was — never an error state, never hidden.
    pub(crate) fn run_series_button_keeps_the_plain_name_when_the_network_call_fails(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));
        runtime.block_on(async {
            Mock::given(method("GET")).and(path("/api/libraries/lib-1/series")).respond_with(ResponseTemplate::new(500)).mount(&mock_server).await;
        });

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item_with_series(&pool, &server.id, "item-1", "Foundation", "Foundation", 3600.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.title_label.label() == "Foundation", Duration::from_secs(5));
        assert!(hooks.series_button.is_visible());
        assert_eq!(hooks.series_button_label.label(), "Foundation");

        // Give Pass 2 a real chance to run and fail; the label must be unchanged afterward.
        // (`false` never becomes true, so `pump_until` keeps pumping the main loop for the full
        // timeout rather than returning on its very first, trivially-true check.)
        pump_until(|| false, Duration::from_millis(500));
        assert_eq!(hooks.series_button_label.label(), "Foundation", "a failed network call must never regress a working local render");
        assert!(hooks.series_button.is_visible());
    }

    /// An item with no series at all never shows the button, and never makes a series-list
    /// request in the first place.
    pub(crate) fn run_series_button_stays_hidden_when_the_item_has_no_series(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(MockServer::start());
        runtime.block_on(mock_item_with_chapters(&mock_server, "item-1", 3600.0, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "No Series Book", None, None, None, 3600.0));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let screen = build(pool.clone(), server, account, session, test_download_manager(pool.clone()), test_controller(pool.clone()), "item-1".to_string(), |_, _| {}, || {}, || {}, |_| {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| hooks.title_label.label() == "No Series Book", Duration::from_secs(5));
        assert!(!hooks.series_button.is_visible());

        assert_eq!(runtime.block_on(mock_server.received_requests()).unwrap().iter().filter(|r| r.url.path().contains("/series")).count(), 0, "no series request should ever be made for a series-less item");
    }

    fn for_each_descendant(root: &gtk4::Widget, f: &mut dyn FnMut(&gtk4::Widget)) {
        let mut child = root.first_child();
        while let Some(widget) = child {
            f(&widget);
            for_each_descendant(&widget, f);
            child = widget.next_sibling();
        }
    }

    fn button_text(button: &gtk4::Button) -> Option<String> {
        if let Some(label) = button.label() {
            return Some(label.to_string());
        }
        let child = button.child()?;
        let mut text = None;
        for_each_descendant(&child, &mut |widget| {
            if text.is_none() {
                if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
                    text = Some(label.label().to_string());
                }
            }
        });
        text
    }

    fn click_button_labeled(container: &gtk4::Box, label: &str) {
        let mut found = None;
        for_each_descendant(container.upcast_ref(), &mut |widget| {
            if found.is_some() {
                return;
            }
            if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
                if button_text(button).as_deref() == Some(label) {
                    found = Some(button.clone());
                }
            }
        });
        found.unwrap_or_else(|| panic!("no button labeled {label:?} found")).emit_clicked();
    }
}
