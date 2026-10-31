//! The shared "Download" scope-picker button — `docs/design/ui-spec.md`'s Item Detail "Download
//! options (Lissen-style)" bottom sheet, approximated as a `GtkMenuButton` + `GtkPopover` at this
//! crate's libadwaita `v1_2` ceiling (`AdwBottomSheet` needs 1.6) — same convention every other
//! popover in this crate already uses instead of `GMenu`/`GAction`.
//!
//! Extracted out of `screens::player` (see its git history) once `screens::item_detail` needed the
//! *identical* behavior: both screens need a user to be able to pick "Current chapter"/"Next
//! chapters" (with its stepper)/"Remaining chapters"/"Entire book", see live size estimates and a
//! free-space guard, and clear whatever's already downloaded — one definition, not two copies that
//! could quietly drift apart. Both call sites build one of these and embed `.widget` directly in
//! their own header/actions row, the same "shared building block" shape `widgets::cover_image` and
//! `widgets::item_card` already are.
//!
//! Never imports `abs_api` directly: chapters are taken as plain `(start_seconds, end_seconds)`
//! pairs rather than `abs_api::ChapterRef` — same boundary `abs_core::download_tracks`'s own
//! `estimate_bytes_for_chapters`/`chapter_offline_markers_for_item` already draw for exactly this
//! reason.

use std::cell::Cell;
use std::rc::Rc;

use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use abs_core::download_tracks::OfflineAvailability;
use abs_core::downloads::DownloadScope;
use abs_core::tracks::TrackRef;

use crate::downloads::{DownloadEvent, DownloadManager, ItemDownloadState};
use crate::i18n::{ntr, tr, tr_args, tr_noop};

/// The button + its popover. Callers embed `.widget` (a `GtkMenuButton`) wherever their layout
/// wants a download action — Player's secondary control row, Item Detail's actions row.
/// `Clone`, so a caller that needs to call [`Self::refresh`] later (from a callback fired after
/// this was built) can hand out a clone while still keeping the original for layout/`TestHooks` —
/// every field is itself a cheap, refcounted handle (`Rc`/GTK's own GObject wrappers), the same
/// clone-is-cheap contract every other widget handle in this crate already has.
#[derive(Clone)]
pub struct DownloadScopeMenu {
    pub widget: gtk4::MenuButton,
    pub(crate) popover: gtk4::Popover,
    /// Rebuilds the popover's rows from whatever `chapter_ranges`/`chapters_ready`/etc. report
    /// *right now* — shared between `connect_show` (below) and [`Self::refresh`], so a caller
    /// whose chapters/tracks arrive after the popover is already open can push the update in
    /// without the popover being closed and reopened. See `populate_download_popover_rows`'s own
    /// doc for what actually renders while chapters aren't ready yet.
    repopulate: Rc<dyn Fn()>,
    #[cfg(test)]
    pub popover_box: gtk4::Box,
}

impl DownloadScopeMenu {
    /// Re-renders the popover's rows if — and only if — it's currently open. A caller whose
    /// chapters/tracks/availability changed after the popover was already showing (Item Detail's
    /// Pass 2 landing after a quick tap) calls this so the popover updates in place; calling it
    /// while the popover is closed would just waste the work; the very next `connect_show` in
    /// `build` below re-populates unconditionally anyway.
    pub fn refresh(&self) {
        if self.popover.is_visible() {
            (self.repopulate)();
        }
    }
}

/// Builds the menu. `chapter_ranges`/`current_chapter_index`/`free_space` are all asked fresh
/// every time the popover is about to open (not captured once at build time) — so a caller whose
/// chapters aren't known yet at construction (Item Detail, which resolves them from the network
/// after this menu already needs to exist) can hand back whatever it has *at open time* rather
/// than forcing the caller to delay building this widget at all. Player's own chapters are fixed
/// once playback starts, so its closure just clones the same list every time — same contract the
/// pre-extraction Player-only version already had for `current_chapter_index`/`free_space`,
/// extended to `chapter_ranges` for this reason.
///
/// `chapters_ready` tells the popover whether `chapter_ranges`/`current_chapter_index` are
/// actually meaningful yet. Item Detail resolves chapters from the network after this menu
/// already needs to exist, and *before* that lands, `chapter_ranges()` returns an empty `Vec` —
/// indistinguishable, from this widget's side alone, from a book that genuinely has no chapters,
/// which every scope row's estimate (and the download manager's own fallback) treats as "the
/// whole book". Opening the menu in that gap used to show every option at the full-book size and
/// clamp "Next chapters" to 1, and "Current chapter" was still tappable — a real full-book
/// download when only the current chapter was wanted. `chapters_ready() == false` instead shows
/// a "Loading chapters…" placeholder with no scope rows at all (see
/// `populate_download_popover_rows`), and the caller calls [`DownloadScopeMenu::refresh`] once
/// its chapters actually land so an already-open popover updates in place. Player's chapters are
/// already known before this is ever built, so its closure just returns `true`.
#[allow(clippy::too_many_arguments)]
pub fn build(
    pool: SqlitePool,
    download_manager: DownloadManager,
    session: abs_core::auth::Session,
    item_id: String,
    chapter_ranges: impl Fn() -> Vec<(f64, f64)> + 'static,
    chapters_ready: impl Fn() -> bool + 'static,
    current_chapter_index: impl Fn() -> usize + 'static,
    free_space: impl Fn() -> Option<u64> + 'static,
    toast_overlay: adw::ToastOverlay,
    // Lets "Download started" carry a "View" action straight to the Downloads tab — the one
    // place with any real progress/cancel affordance before the download finishes, and otherwise
    // unreachable from here: both Item Detail and the Player are content-swapped over the shell,
    // hiding the tab bar entirely. `Rc`, not a plain closure, since it's cloned into two toast
    // sites below and into every popover reopen.
    on_open_downloads: Rc<dyn Fn()>,
) -> DownloadScopeMenu {
    let popover_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    let popover = gtk4::Popover::builder().child(&popover_box).build();
    let widget = gtk4::MenuButton::builder().icon_name("folder-download-symbolic").tooltip_text(tr("Download")).popover(&popover).build();
    crate::a11y::name(&widget, &tr("Download"));

    // Shared by `connect_show` below and `DownloadScopeMenu::refresh` — rebuilding whether
    // "Clear downloaded chapters" applies, the size estimates, the free-space guard, and now
    // whether chapters are ready at all, since every one of those can change between opens (or,
    // for chapters, while the popover is already open).
    // Guards against a *stale* repopulate's async pass landing after a newer repopulate call
    // already rendered fresher rows — a real race, not just a theoretical one: `refresh()` is
    // called right as chapters become ready, which for a locally-cached seed (no network wait at
    // all) can land within milliseconds of the popover's own initial `connect_show`-triggered
    // populate, whose async pass (also a local DB read) may not have completed yet. Each
    // `repopulate()` call claims the next generation; its async pass only applies what it found
    // if its generation is still the current one by the time it lands.
    let generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));

    let repopulate: Rc<dyn Fn()> = Rc::new({
        let pool = pool.clone();
        let download_manager = download_manager.clone();
        let popover_box = popover_box.clone();
        let popover_for_rows = popover.clone();
        let session = session.clone();
        let item_id = item_id.clone();
        let toast_overlay = toast_overlay.clone();
        let on_open_downloads = on_open_downloads.clone();
        let generation = generation.clone();
        move || {
            let my_generation = generation.get() + 1;
            generation.set(my_generation);
            // The stepper's count defaults to 10 (ui-spec) on every (re)population and is shared
            // by both populate passes below, so the async availability rebuild can't reset a
            // count the user already stepped.
            let ready = chapters_ready();
            let chapter_ranges = chapter_ranges();
            let current_chapter_index = current_chapter_index();
            // Free space is one cheap statvfs — available even to the synchronous first pass.
            let free_space = free_space();
            let next_count = Rc::new(Cell::new(10u32));
            populate_download_popover_rows(
                &popover_box,
                ready,
                OfflineAvailability::None,
                &download_manager,
                &popover_for_rows,
                &session,
                &item_id,
                current_chapter_index,
                &chapter_ranges,
                &[],
                None,
                free_space,
                &next_count,
                &toast_overlay,
                &on_open_downloads,
            );
            glib::spawn_future_local({
                let pool = pool.clone();
                let popover_box = popover_box.clone();
                let download_manager = download_manager.clone();
                let popover_for_rows = popover_for_rows.clone();
                let session = session.clone();
                let item_id = item_id.clone();
                let chapter_ranges = chapter_ranges.clone();
                let next_count = next_count.clone();
                let toast_overlay = toast_overlay.clone();
                let on_open_downloads = on_open_downloads.clone();
                let generation = generation.clone();
                async move {
                    let server_id = session.server_id().to_string();
                    let availability = abs_core::download_tracks::item_offline_availability(&pool, &server_id, &item_id).await.unwrap_or(OfflineAvailability::None);
                    let tracks = abs_core::tracks::cached_tracks(&pool, &server_id, &item_id).await.unwrap_or_default();
                    // The book's total size from the library list: an estimate for "Entire book"
                    // until the files' own sizes are cached.
                    let book_size = abs_storage::repo::items::get_extras(&pool, &server_id, &item_id).await.ok().and_then(|extras| extras.size_bytes).and_then(|size| u64::try_from(size).ok());
                    if generation.get() != my_generation {
                        // A newer `repopulate()` call has already started (and, since its own
                        // synchronous pass runs before any `.await` point, already rendered) —
                        // applying this stale result now would clobber it with what could easily
                        // be older, wronger state (e.g. `chapters_ready() == false` from before a
                        // caller's `refresh()`).
                        return;
                    }
                    populate_download_popover_rows(
                        &popover_box,
                        ready,
                        availability,
                        &download_manager,
                        &popover_for_rows,
                        &session,
                        &item_id,
                        current_chapter_index,
                        &chapter_ranges,
                        &tracks,
                        book_size,
                        free_space,
                        &next_count,
                        &toast_overlay,
                        &on_open_downloads,
                    );
                }
            });
        }
    });

    popover.connect_show({
        let repopulate = repopulate.clone();
        move |_| repopulate()
    });

    // Reflects this item's overall download state on the button's own icon (ui-spec: idle ->
    // in-progress -> checkmark). Registered once, for the manager's whole lifetime, filtered on
    // the item id fixed at construction — unlike the pre-extraction Player-only version, which
    // filtered against `PlayerController::current_download_context()` (the *currently loaded*
    // item) because one `PlayerScreen`'s listener stayed registered while the mini-bar could
    // in principle switch items in the background. A `DownloadScopeMenu` is instead built fresh
    // per screen-open (Player, on every mini-bar tap) or per item (Item Detail), so its own item
    // id never actually changes across the instance's lifetime — matching `DownloadEvent` itself,
    // which carries no server id to disambiguate further.
    download_manager.add_scoped_listener({
        // Weak: the screen this button lives on comes and goes, and the manager outlives it.
        let widget = widget.downgrade();
        let item_id = item_id.clone();
        let toast_overlay = toast_overlay.downgrade();
        move |event| {
            let (Some(widget), Some(toast_overlay)) = (widget.upgrade(), toast_overlay.upgrade()) else { return false };
            let DownloadEvent::ItemStateChanged { item_id: event_item_id, state } = event else { return true };
            if *event_item_id != item_id {
                return true;
            }
            let (icon, name) = match state {
                ItemDownloadState::Downloading => ("content-loading-symbolic", tr("Downloading")),
                ItemDownloadState::Complete => ("object-select-symbolic", tr("Downloaded")),
                // A stopped download kept its completed chapters — the button returns to its
                // "can start/continue a download" state, same as idle.
                ItemDownloadState::Idle | ItemDownloadState::Stopped | ItemDownloadState::Failed(_) => ("folder-download-symbolic", tr("Download")),
            };
            widget.set_icon_name(icon);
            crate::a11y::name_if_changed(&widget, &name);
            // Before this, a failure only ever showed as the icon quietly reverting from its
            // in-progress spinner to idle a moment after "Download started" — indistinguishable
            // from Stop, and with the actual reason (metered connection, dead session, a 404)
            // sitting unread in the log.
            if let ItemDownloadState::Failed(reason) = state {
                toast_overlay.add_toast(adw::Toast::new(&tr_args("Download failed — {reason}", &[("reason", reason)])));
            }
            true
        }
    });

    DownloadScopeMenu {
        widget,
        popover,
        repopulate,
        #[cfg(test)]
        popover_box,
    }
}

/// What a download asked for while offline mode is on gets instead of a download.
pub(crate) const OFFLINE_DOWNLOAD_TOAST: &str = tr_noop("Offline mode is on — turn it off to download");

/// Estimated bytes a scope would fetch, or `None` when no honest estimate exists: sizes not
/// cached yet, or nothing to fetch (a disabled row shouldn't advertise "≈0 B"). No chapters at
/// all means every scope degenerates to the whole book — the same fallback
/// `DownloadManager::start_download` applies — so the estimate is every cached track.
fn estimate_bytes_for(tracks: &[TrackRef], chapter_ranges: &[(f64, f64)], scope: DownloadScope, current_chapter_index: usize, book_size: Option<u64>) -> Option<u64> {
    let from_files = estimate_bytes_from_files(tracks, chapter_ranges, scope, current_chapter_index);
    // The files aren't listed yet (a book never opened or downloaded before): the book's total
    // size from the library list stands in for the whole-book scope only. It counts every file of
    // the item, a cover or an e-book included, so it can read a little high, which "≈" allows.
    match (from_files, scope) {
        (None, DownloadScope::EntireBook) => book_size.filter(|size| *size > 0),
        (estimate, _) => estimate,
    }
}

fn estimate_bytes_from_files(tracks: &[TrackRef], chapter_ranges: &[(f64, f64)], scope: DownloadScope, current_chapter_index: usize) -> Option<u64> {
    if chapter_ranges.is_empty() {
        if tracks.is_empty() || tracks.iter().any(|t| t.size_bytes.is_none()) {
            return None;
        }
        return Some(tracks.iter().map(|t| t.size_bytes.unwrap_or(0)).sum());
    }
    let indices = abs_core::downloads::resolve_scope(scope, current_chapter_index, chapter_ranges.len());
    if indices.is_empty() {
        return None;
    }
    abs_core::download_tracks::estimate_bytes_for_chapters(tracks, chapter_ranges, &indices)
}

/// The spec's estimate text ("≈340 MB").
fn estimate_text(bytes: u64) -> String {
    // TRANSLATORS: {size} is a data size such as "340 MB"; "≈" means approximately.
    tr_args("≈{size}", &[("size", &crate::screens::downloads::format_bytes(bytes))])
}

/// The spec's estimate subtitle ("≈340 MB") — dim, on its own line under the row's title.
fn estimate_label(bytes: u64) -> gtk4::Label {
    gtk4::Label::builder()
        .label(estimate_text(bytes))
        .css_classes(["dim-label"])
        .xalign(0.0)
        .build()
}

/// The spec's "Not enough free space" subtitle — the row's estimate exceeds the free space the
/// manager just measured, so the row refuses to start anything (it toasts instead).
fn blocked_label() -> gtk4::Label {
    gtk4::Label::builder()
        .label(tr("Not enough free space"))
        .css_classes(["error"])
        .xalign(0.0)
        .build()
}

/// The "Download started" toast, with a "View" action to the Downloads tab — otherwise
/// unreachable from here (both Item Detail and the Player are content-swapped over the shell,
/// hiding the tab bar), and the only screen with any progress/cancel affordance before this
/// screen's own `widgets::download_progress::DownloadProgressStrip` reveals a moment later.
fn started_download_toast(on_open_downloads: &Rc<dyn Fn()>) -> adw::Toast {
    let toast = adw::Toast::new(&tr("Download started"));
    toast.set_button_label(Some(&tr("View")));
    toast.connect_button_clicked({
        let on_open_downloads = on_open_downloads.clone();
        move |_| on_open_downloads()
    });
    toast
}

/// (Re)builds the download button's popover rows: the four scope options from ui-spec's Item
/// Detail download sheet — "Current chapter", "Next chapters" with its inline − / count / +
/// stepper (default 10, clamped to the chapters actually remaining after the current one; tapping
/// the row body starts the download for the stepper's count), "Remaining chapters", "Entire book"
/// — plus a destructive "Clear downloaded chapters" row shown only once `availability` says
/// something is actually downloaded. Each scope row carries the spec's size-estimate subtitle
/// ("≈340 MB"), computed offline from the cached per-track sizes (`tracks` — empty means sizes
/// aren't synced yet, so no subtitle is shown at all rather than a wrong one). `next_count` is
/// shared with the caller so the popover's synchronous and async populate passes (same open)
/// can't clobber a count the user already stepped.
///
/// `chapters_ready: false` replaces all four scope rows and the stepper with a single "Loading
/// chapters…" placeholder — no scope row exists at all, so nothing can be tapped into starting a
/// download sized for the whole book when only a chapter's worth was ever asked for (see
/// `build`'s own doc for the report this fixes). The Stop-download row (synchronous, needs no
/// chapters) and the Clear-downloaded-chapters row (only known once the async availability read
/// lands, independent of chapters) are unaffected either way.
#[allow(clippy::too_many_arguments)]
fn populate_download_popover_rows(
    popover_box: &gtk4::Box,
    chapters_ready: bool,
    availability: OfflineAvailability,
    download_manager: &DownloadManager,
    popover: &gtk4::Popover,
    session: &abs_core::auth::Session,
    item_id: &str,
    current_chapter_index: usize,
    chapter_ranges: &[(f64, f64)],
    tracks: &[TrackRef],
    book_size: Option<u64>,
    free_space: Option<u64>,
    next_count: &Rc<Cell<u32>>,
    toast_overlay: &adw::ToastOverlay,
    on_open_downloads: &Rc<dyn Fn()>,
) {
    while let Some(child) = popover_box.first_child() {
        popover_box.remove(&child);
    }

    // The spec titles this sheet "Download book" (test-plan ID-6). A real AdwBottomSheet needs
    // libadwaita 1.6 and this crate's ceiling is 1.2, so the popover carries the title as a
    // heading row instead — rebuilt here (not once at construction) so it survives the clear
    // below on every re-open.
    popover_box.append(&gtk4::Label::builder().label(tr("Download book")).css_classes(["heading"]).halign(gtk4::Align::Start).margin_bottom(4).build());

    // Estimated bytes for a scope — the same chapter->track mapping the actual download uses, so
    // the estimate is of exactly what would be fetched. The fallbacks (no chapters at all ->
    // whole book; nothing to fetch or no cached sizes -> no estimate) live in
    // `estimate_bytes_for` above.
    let estimate_for = |scope: DownloadScope| estimate_bytes_for(tracks, chapter_ranges, scope, current_chapter_index, book_size);

    let add_scope_row = |label: &str, scope: DownloadScope| {
        let inner = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(2).halign(gtk4::Align::Start).build();
        inner.append(&gtk4::Label::builder().label(label).xalign(0.0).build());
        // ui-spec: an option whose estimate exceeds free space switches its subtitle to an error
        // tint reading "Not enough free space" instead of letting the download start and fail
        // partway through. Unknown free space (or unknown size) never blocks anything.
        let blocked = matches!((estimate_for(scope), free_space), (Some(bytes), Some(free)) if bytes > free);
        match (estimate_for(scope), blocked) {
            (Some(bytes), false) => inner.append(&estimate_label(bytes)),
            (Some(_), true) => inner.append(&blocked_label()),
            (None, _) => {}
        }
        let button = gtk4::Button::builder().child(&inner).css_classes(["flat"]).build();
        // A screen reader takes nothing from the labels inside the button.
        crate::a11y::name(&button, label);
        match (estimate_for(scope), blocked) {
            (Some(bytes), false) => crate::a11y::describe(&button, &estimate_text(bytes)),
            (Some(_), true) => crate::a11y::describe(&button, &tr("Not enough free space")),
            (None, _) => {}
        }
        button.connect_clicked({
            let download_manager = download_manager.clone();
            let popover = popover.clone();
            let session = session.clone();
            let item_id = item_id.to_string();
            let toast_overlay = toast_overlay.clone();
            let on_open_downloads = on_open_downloads.clone();
            move |_| {
                if session.is_offline() {
                    popover.popdown();
                    toast_overlay.add_toast(adw::Toast::new(&tr(OFFLINE_DOWNLOAD_TOAST)));
                    return;
                }
                if blocked {
                    toast_overlay.add_toast(adw::Toast::new(&tr("Not enough free space")));
                    return;
                }
                download_manager.start_download(session.clone(), item_id.clone(), scope, current_chapter_index);
                popover.popdown();
                toast_overlay.add_toast(started_download_toast(&on_open_downloads));
            }
        });
        popover_box.append(&button);
    };

    // While a download is in flight for this item, Stop is the first action — it ends the job
    // keeping the chapters that already completed (same semantics as the Downloads screen's
    // stop button), whereas "Clear downloaded chapters" at the bottom deletes everything.
    if download_manager.is_downloading(session.server_id(), item_id) {
        let stop_button = gtk4::Button::builder().label(tr("Stop download")).css_classes(["flat"]).halign(gtk4::Align::Start).build();
        stop_button.connect_clicked({
            let download_manager = download_manager.clone();
            let popover = popover.clone();
            let server_id = session.server_id().to_string();
            let item_id = item_id.to_string();
            let toast_overlay = toast_overlay.clone();
            move |_| {
                download_manager.cancel_item(&server_id, &item_id);
                popover.popdown();
                toast_overlay.add_toast(adw::Toast::new(&tr("Download stopped")));
            }
        });
        popover_box.append(&stop_button);
    }

    if !chapters_ready {
        // No scope row exists at all here — see this function's own doc for why. A spinner
        // rather than a plain label so it's visually obvious this is a transient state, not
        // a book with a "Loading chapters…" row as one of its permanent options.
        let loading_row = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk4::Align::Start)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        loading_row.append(&gtk4::Spinner::builder().spinning(true).build());
        loading_row.append(&gtk4::Label::builder().label(tr("Loading chapters…")).css_classes(["dim-label"]).build());
        popover_box.append(&loading_row);
    } else {
        add_scope_row(&tr("Current chapter"), DownloadScope::CurrentChapter);

        // "Next chapters" is the one scope row with an inline stepper (ui-spec: "− / count / +, each
        // button ≥44×44px per the touch-target note"; the row body outside the stepper starts the
        // download for the stepper's count). The count is clamped to what's actually remaining after
        // the current chapter — on the last chapter there is nothing after it, so the whole row goes
        // insensitive rather than offering a download that could only ever no-op. Its subtitle
        // recomputes on every step, since the count is what the estimate is of.
        let remaining = chapter_ranges.len().saturating_sub(current_chapter_index + 1);
        let next_title = gtk4::Label::builder().label(tr("Next chapters")).xalign(0.0).build();
        let next_subtitle = gtk4::Label::builder().css_classes(["dim-label"]).xalign(0.0).visible(false).build();
        let next_inner = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(2).halign(gtk4::Align::Start).hexpand(true).valign(gtk4::Align::Center).build();
        next_inner.append(&next_title);
        next_inner.append(&next_subtitle);
        let next_button = gtk4::Button::builder().child(&next_inner).css_classes(["flat"]).build();
        crate::a11y::name(&next_button, &tr("Next chapters"));
        let minus_button = gtk4::Button::builder().label("−").css_classes(["flat"]).width_request(44).height_request(44).build(); // i18n: ignore — a mathematical symbol, not text
        let plus_button = gtk4::Button::builder().label("+").css_classes(["flat"]).width_request(44).height_request(44).build(); // i18n: ignore — a mathematical symbol, not text
        crate::a11y::name(&minus_button, &tr("Fewer chapters"));
        crate::a11y::name(&plus_button, &tr("More chapters"));
        let count_label = gtk4::Label::builder().width_chars(3).justify(gtk4::Justification::Center).build();
        count_label.set_accessible_role(gtk4::AccessibleRole::Status);
        // Whether the Next-chapters estimate currently exceeds free space — flipped by the subtitle
        // update (which knows) and read by the row's click handler, so stepping into or out of the
        // blocked range changes what tapping the row does.
        let next_blocked = Rc::new(Cell::new(false));

        let update_count_label: Rc<dyn Fn()> = {
            let count_label = count_label.clone();
            let next_count = next_count.clone();
            let minus_button = minus_button.clone();
            let plus_button = plus_button.clone();
            Rc::new(move || {
                let count = next_count.get();
                count_label.set_label(&count.to_string());
                // A screen-reader user steps with focus on − or +, so the count is read there.
                let spoken = ntr("{count} chapter", "{count} chapters", count);
                crate::a11y::describe(&minus_button, &spoken);
                crate::a11y::describe(&plus_button, &spoken);
            })
        };
        let update_next_subtitle: Rc<dyn Fn()> = {
            let next_subtitle = next_subtitle.clone();
            let next_button = next_button.clone();
            let next_count = next_count.clone();
            let next_blocked = next_blocked.clone();
            let tracks = tracks.to_vec();
            let chapter_ranges = chapter_ranges.to_vec();
            Rc::new(move || {
                let scope = DownloadScope::NextChapters(next_count.get());
                match (estimate_bytes_for(tracks.as_slice(), chapter_ranges.as_slice(), scope, current_chapter_index, None), free_space) {
                    (Some(bytes), Some(free)) if bytes > free => {
                        next_subtitle.set_label(&tr("Not enough free space"));
                        next_subtitle.remove_css_class("dim-label");
                        next_subtitle.add_css_class("error");
                        next_subtitle.set_visible(true);
                        next_blocked.set(true);
                    }
                    (Some(bytes), _) => {
                        next_subtitle.set_label(&estimate_text(bytes));
                        next_subtitle.remove_css_class("error");
                        next_subtitle.add_css_class("dim-label");
                        next_subtitle.set_visible(true);
                        next_blocked.set(false);
                    }
                    (None, _) => {
                        next_subtitle.set_visible(false);
                        next_blocked.set(false);
                    }
                }
                let spoken = if next_subtitle.is_visible() { next_subtitle.label().to_string() } else { String::new() };
                crate::a11y::describe(&next_button, &spoken);
            })
        };
        next_count.set(next_count.get().clamp(1, remaining.max(1) as u32));
        update_count_label();
        update_next_subtitle();

        {
            let next_count = next_count.clone();
            let update_count_label = update_count_label.clone();
            let update_next_subtitle = update_next_subtitle.clone();
            minus_button.connect_clicked(move |_| {
                next_count.set(next_count.get().saturating_sub(1).max(1));
                update_count_label();
                update_next_subtitle();
            });
        }
        {
            let next_count = next_count.clone();
            let update_count_label = update_count_label.clone();
            let update_next_subtitle = update_next_subtitle.clone();
            plus_button.connect_clicked(move |_| {
                next_count.set((next_count.get() + 1).min(remaining.max(1) as u32));
                update_count_label();
                update_next_subtitle();
            });
        }

        next_button.connect_clicked({
            let download_manager = download_manager.clone();
            let popover = popover.clone();
            let session = session.clone();
            let item_id = item_id.to_string();
            let toast_overlay = toast_overlay.clone();
            let next_count = next_count.clone();
            let next_blocked = next_blocked.clone();
            let on_open_downloads = on_open_downloads.clone();
            move |_| {
                if session.is_offline() {
                    popover.popdown();
                    toast_overlay.add_toast(adw::Toast::new(&tr(OFFLINE_DOWNLOAD_TOAST)));
                    return;
                }
                if next_blocked.get() {
                    toast_overlay.add_toast(adw::Toast::new(&tr("Not enough free space")));
                    return;
                }
                download_manager.start_download(session.clone(), item_id.clone(), DownloadScope::NextChapters(next_count.get()), current_chapter_index);
                popover.popdown();
                toast_overlay.add_toast(started_download_toast(&on_open_downloads));
            }
        });

        if remaining == 0 {
            next_button.set_sensitive(false);
            minus_button.set_sensitive(false);
            plus_button.set_sensitive(false);
            count_label.add_css_class("dim-label");
        }

        let stepper = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(4).build();
        stepper.append(&minus_button);
        stepper.append(&count_label);
        stepper.append(&plus_button);

        let next_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).build();
        next_row.append(&next_button);
        next_row.append(&stepper);
        popover_box.append(&next_row);

        add_scope_row(&tr("Remaining chapters"), DownloadScope::RemainingChapters);
        add_scope_row(&tr("Entire book"), DownloadScope::EntireBook);
    }

    if availability != OfflineAvailability::None {
        let clear_button = gtk4::Button::builder().label(tr("Clear downloaded chapters")).css_classes(["destructive-action"]).margin_top(6).build();
        clear_button.connect_clicked({
            let download_manager = download_manager.clone();
            let popover = popover.clone();
            let session = session.clone();
            let item_id = item_id.to_string();
            let toast_overlay = toast_overlay.clone();
            move |_| {
                popover.popdown();
                let toast_overlay = toast_overlay.clone();
                download_manager.clear_item(session.server_id(), &item_id, move |result| match result {
                    Ok(()) => toast_overlay.add_toast(adw::Toast::new(&tr("Downloaded chapters cleared"))),
                    Err(err) => crate::error_reporting::report_background_error(&toast_overlay, &tr("Clearing downloaded chapters"), err),
                });
            }
        });
        popover_box.append(&clear_button);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::player::PlayRequest;
    use crate::test_support::{any_label_reads, pump_until};
    use std::time::Duration;

    fn test_download_manager(pool: sqlx::SqlitePool) -> DownloadManager {
        DownloadManager::new(pool, crate::test_support::test_paths(), Box::new(abs_player::network_watch::UnknownNetworkMonitor), false)
    }

    /// A window is needed for `popover.popup()` to actually show anything (a `GtkPopover` needs a
    /// realized/mapped toplevel ancestor) — every scenario below builds one around the widget.
    fn mapped_window(widget: &gtk4::MenuButton) -> gtk4::Window {
        let window = gtk4::Window::builder().child(widget).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));
        window
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Regression test for opening the menu
    /// before chapters are known: no scope row (not even "Current chapter") may exist while
    /// `chapters_ready` reports `false` — a "Loading chapters…" placeholder stands in for all
    /// four instead — since every scope's estimate degenerates to "the whole book" without real
    /// chapters, and a book-sized download must never be one accidental tap away. Once the caller
    /// flips `chapters_ready` and calls `refresh()`, the still-open popover must show the real
    /// rows in place, with no reopen needed.
    pub(crate) fn run_shows_a_loading_placeholder_until_chapters_are_ready(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::downloads::tests::mock_three_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-1", &[
            abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 5.0, offset_seconds: 0.0, size_bytes: Some(1_000_000) },
            abs_storage::repo::tracks::NewTrack { ino: "2", duration_seconds: 5.0, offset_seconds: 5.0, size_bytes: Some(2_000_000) },
            abs_storage::repo::tracks::NewTrack { ino: "3", duration_seconds: 5.0, offset_seconds: 10.0, size_bytes: Some(4_000_000) },
        ]))
        .unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let chapter_ranges = vec![(0.0, 5.0), (5.0, 10.0), (10.0, 15.0)];
        let ready = Rc::new(Cell::new(false));
        let download_manager = test_download_manager(pool.clone());
        let toast_overlay = adw::ToastOverlay::new();
        let menu = build(
            pool.clone(),
            download_manager,
            session,
            "item-1".to_string(),
            move || chapter_ranges.clone(),
            {
                let ready = ready.clone();
                move || ready.get()
            },
            || 0,
            || None,
            toast_overlay,
            Rc::new(|| {}),
        );

        let window = mapped_window(&menu.widget);
        menu.popover.popup();
        pump_until(|| menu.popover_box.first_child().is_some(), Duration::from_secs(2));

        assert!(
            for_each_descendant_labels(&menu.popover_box).iter().any(|t| t == "Loading chapters…"),
            "chapters not ready yet should show the loading placeholder"
        );
        assert!(button_labeled(&menu.popover_box, "Current chapter").is_none(), "no scope row must exist while chapters aren't ready");
        assert!(button_labeled(&menu.popover_box, "Entire book").is_none(), "no scope row must exist while chapters aren't ready");

        ready.set(true);
        menu.refresh();
        pump_until(
            || estimate_texts(&menu.popover_box) == ["≈1.0 MB".to_string(), "≈6.0 MB".to_string(), "≈7.0 MB".to_string(), "≈7.0 MB".to_string()],
            Duration::from_secs(2),
        );

        assert!(
            !for_each_descendant_labels(&menu.popover_box).iter().any(|t| t == "Loading chapters…"),
            "the loading placeholder must be gone once chapters are ready"
        );
        assert!(button_labeled(&menu.popover_box, "Current chapter").is_some(), "the real scope rows must appear in the still-open popover");
        let (_, count, _) = stepper_widgets(&menu.popover_box);
        assert_eq!(count.label(), "2", "the stepper should reflect the real chapter count once ready, not the loading-state fallback of 1");

        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The "Next chapters" stepper must
    /// behave per ui-spec (ID-8): default 10, ±1 per tap, clamped to the chapters remaining after
    /// the current one (so a default of 10 displays as 9 for ten chapters at the first one), and
    /// floored at 1; both stepper buttons are 44×44px touch targets.
    pub(crate) fn run_stepper_defaults_clamps_and_steps(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        let titles: Vec<String> = (0..10).map(|i| format!("C{i}")).collect();
        let chapters: Vec<(&str, f64, f64)> = titles.iter().enumerate().map(|(i, t)| (t.as_str(), i as f64, i as f64 + 1.0)).collect();
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(&mock_server, "item-1", 10, &chapters));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), crate::player::tests::test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Chaptered Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.chapters().len() == 10, Duration::from_secs(10));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let chapter_ranges: Vec<(f64, f64)> = controller.chapters().iter().map(|c| (c.start_seconds, c.end_seconds)).collect();
        let download_manager = test_download_manager(pool.clone());
        let toast_overlay = adw::ToastOverlay::new();
        let menu = build(pool.clone(), download_manager, session, "item-1".to_string(), move || chapter_ranges.clone(), || true, || 0, || None, toast_overlay, Rc::new(|| {}));

        let window = mapped_window(&menu.widget);
        menu.popover.popup();
        pump_until(|| menu.popover_box.first_child().is_some(), Duration::from_secs(2));

        let (minus, count, plus) = stepper_widgets(&menu.popover_box);
        assert_eq!(count.label(), "9", "the default 10 must clamp to the 9 chapters remaining after the current one");
        assert_eq!(minus.width_request(), 44, "stepper buttons need their own 44px touch target");
        assert_eq!(minus.height_request(), 44);
        assert_eq!(plus.width_request(), 44);
        assert_eq!(plus.height_request(), 44);

        plus.emit_clicked();
        assert_eq!(count.label(), "9", "+ at the remaining-chapters ceiling must be a no-op");

        for expected in (1..=8).rev() {
            minus.emit_clicked();
            assert_eq!(count.label(), expected.to_string());
        }
        minus.emit_clicked();
        assert_eq!(count.label(), "1", "− must floor at 1 (a scope of zero chapters is not a choice)");

        window.destroy();
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Tapping the "Next chapters" row body
    /// must download exactly the stepper's count of chapters (ui-spec ID-9): on a two-chapter,
    /// two-file item at the first chapter the clamped count is 1, so only the *second* chapter's
    /// track may ever be fetched.
    pub(crate) fn run_next_chapters_row_uses_the_stepper_count(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::downloads::tests::mock_two_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let chapter_ranges = vec![(0.0, 5.0), (5.0, 10.0)];
        let download_manager = test_download_manager(pool.clone());
        let toast_overlay = adw::ToastOverlay::new();
        let menu = build(pool.clone(), download_manager, session, "item-1".to_string(), move || chapter_ranges.clone(), || true, || 0, || None, toast_overlay, Rc::new(|| {}));

        let window = mapped_window(&menu.widget);
        menu.popover.popup();
        pump_until(|| menu.popover_box.first_child().is_some(), Duration::from_secs(2));
        let (_, count, _) = stepper_widgets(&menu.popover_box);
        assert_eq!(count.label(), "1", "only chapter 2 remains after chapter 1, so the stepper clamps to 1");
        click_button_labeled(&menu.popover_box, "Next chapters");

        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "2")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );
        pump_until(|| menu.widget.icon_name().as_deref() == Some("object-select-symbolic"), Duration::from_secs(5));
        assert!(
            runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().is_none(),
            "chapter 1's track must not be fetched — the download is exactly the stepper's 1 chapter"
        );

        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Every scope row must show the spec's
    /// size-estimate subtitle computed from the cached per-file sizes (ID-7), and the "Next
    /// chapters" subtitle must track the stepper (three 1/2/4 MB files at the first chapter:
    /// default count clamps to 2 -> ≈6.0 MB; one step down -> just the second file's ≈2.0 MB).
    pub(crate) fn run_rows_show_size_estimates_that_track_the_stepper(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::downloads::tests::mock_three_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        // The popover's estimates read the *cached* track sizes, which the download manager
        // normally writes on an item's first download — seeded directly here so the test
        // exercises the popover's read side without a download round-trip.
        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-1", &[
            abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 5.0, offset_seconds: 0.0, size_bytes: Some(1_000_000) },
            abs_storage::repo::tracks::NewTrack { ino: "2", duration_seconds: 5.0, offset_seconds: 5.0, size_bytes: Some(2_000_000) },
            abs_storage::repo::tracks::NewTrack { ino: "3", duration_seconds: 5.0, offset_seconds: 10.0, size_bytes: Some(4_000_000) },
        ]))
        .unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let chapter_ranges = vec![(0.0, 5.0), (5.0, 10.0), (10.0, 15.0)];
        let download_manager = test_download_manager(pool.clone());
        let toast_overlay = adw::ToastOverlay::new();
        let menu = build(pool.clone(), download_manager, session, "item-1".to_string(), move || chapter_ranges.clone(), || true, || 0, || None, toast_overlay, Rc::new(|| {}));

        let window = mapped_window(&menu.widget);
        menu.popover.popup();
        // Two-stage render: the synchronous first pass has no cached track sizes yet (no
        // subtitles), the async pass fetches them from the DB — so pump until the estimates are
        // the *correct* ones, not merely present.
        pump_until(|| estimate_texts(&menu.popover_box) == ["≈1.0 MB".to_string(), "≈6.0 MB".to_string(), "≈7.0 MB".to_string(), "≈7.0 MB".to_string()], Duration::from_secs(2));
        assert_eq!(estimate_texts(&menu.popover_box), ["≈1.0 MB", "≈6.0 MB", "≈7.0 MB", "≈7.0 MB"], "current (file 1), next (default count 2: files 2+3), remaining and entire (everything)");

        // ID-6: the sheet is titled "Download book" (as a heading row, the popover stand-in for
        // the spec's AdwBottomSheet).
        assert!(
            for_each_descendant_labels(&menu.popover_box).iter().any(|t| t == "Download book"),
            "the popover should carry the spec's 'Download book' title"
        );

        let (minus, _, _) = stepper_widgets(&menu.popover_box);
        minus.emit_clicked();
        assert_eq!(estimate_texts(&menu.popover_box), ["≈1.0 MB", "≈2.0 MB", "≈7.0 MB", "≈7.0 MB"], "stepping the count down to 1 shrinks the estimate to just file 2");

        window.destroy();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A scope whose estimate exceeds free
    /// space must switch to the spec's error-tinted "Not enough free space" subtitle and refuse to
    /// start (ID-10/ID-16: no download, no partial state) — the sizes here are petabyte-scale
    /// precisely so the assertion can't depend on the test machine's actual free space.
    pub(crate) fn run_rows_block_when_free_space_is_insufficient(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::downloads::tests::mock_three_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        // Same read side the real flow produces (the manager caches these sizes on an item's first
        // download) — but sized like the Library of Alexandria's raw footage.
        runtime.block_on(abs_storage::repo::tracks::upsert_all(&pool, &server.id, "item-1", &[
            abs_storage::repo::tracks::NewTrack { ino: "1", duration_seconds: 5.0, offset_seconds: 0.0, size_bytes: Some(1_000_000_000_000_000) },
            abs_storage::repo::tracks::NewTrack { ino: "2", duration_seconds: 5.0, offset_seconds: 5.0, size_bytes: Some(1_000_000_000_000_000) },
            abs_storage::repo::tracks::NewTrack { ino: "3", duration_seconds: 5.0, offset_seconds: 10.0, size_bytes: Some(1_000_000_000_000_000) },
        ]))
        .unwrap();

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let chapter_ranges = vec![(0.0, 5.0), (5.0, 10.0), (10.0, 15.0)];
        // A fresh paths tempdir whose directories actually exist, since free space is "unknown"
        // (and unknown is treated as unrestricted) for a path that isn't there yet.
        let paths = crate::test_support::test_paths();
        runtime.block_on(paths.ensure_dirs()).unwrap();
        let download_manager = DownloadManager::new(pool.clone(), paths, Box::new(abs_player::network_watch::UnknownNetworkMonitor), false);
        let free_space_manager = download_manager.clone();
        let toast_overlay = adw::ToastOverlay::new();
        let menu = build(pool.clone(), download_manager, session, "item-1".to_string(), move || chapter_ranges.clone(), || true, || 0, move || free_space_manager.free_space_bytes(), toast_overlay, Rc::new(|| {}));

        let window = mapped_window(&menu.widget);
        menu.popover.popup();
        pump_until(|| blocked_row_count(&menu.popover_box) == 4, Duration::from_secs(2));

        click_button_labeled(&menu.popover_box, "Entire book");
        // Let any wrongly-started download land; nothing should (the same pump-a-beat-and-check
        // idiom the player screen's own equivalent test used).
        pump_until(|| false, Duration::from_millis(300));
        let rows = runtime.block_on(abs_storage::repo::download_tracks::list_for_item(&pool, &server.id, "item-1")).unwrap();
        assert!(rows.is_empty(), "a blocked row must not start any download, not even partially");

        window.destroy();
    }

    /// Regression test for the "a download fails and the only sign is the icon quietly reverting"
    /// gap: a metered connection with "Wi-Fi-only downloads" on fails every track immediately
    /// (`FakeNetworkMonitor` reports metered; `wifi_only: true`), and the failure must now surface
    /// as a toast naming the actual reason, not just "Download started" and then silence.
    pub(crate) fn run_a_failed_download_toasts_the_reason(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::downloads::tests::mock_three_track_item(&mock_server, "item-1"));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let chapter_ranges = vec![(0.0, 5.0), (5.0, 10.0), (10.0, 15.0)];
        let paths = crate::test_support::test_paths();
        runtime.block_on(paths.ensure_dirs()).unwrap();
        struct MeteredNetworkMonitor;
        impl abs_player::network_watch::NetworkMonitor for MeteredNetworkMonitor {
            fn is_metered(&self) -> Option<bool> {
                Some(true)
            }
        }
        let download_manager = DownloadManager::new(pool.clone(), paths, Box::new(MeteredNetworkMonitor), true);

        // A window whose *content* is the toast overlay (not just a sibling of it) — an
        // unmapped `AdwToastOverlay` defers its toasts, same requirement
        // `error_reporting::tests` documents for its own scenario.
        let toast_overlay = adw::ToastOverlay::new();
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        toast_overlay.set_child(Some(&content));
        let menu = build(pool.clone(), download_manager, session, "item-1".to_string(), move || chapter_ranges.clone(), || true, || 0, || None, toast_overlay.clone(), Rc::new(|| {}));
        content.append(&menu.widget);
        let window = gtk4::Window::builder().child(&toast_overlay).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        menu.popover.popup();
        // Row population runs in two passes (a synchronous one from the given chapter ranges,
        // then an async one once cached tracks/offline-availability resolve) — waiting on
        // `is_visible()` alone can race ahead of both, the same reason
        // `run_rows_block_when_free_space_is_insufficient` waits on the rows' own content instead.
        pump_until(|| button_labeled(&menu.popover_box, "Current chapter").is_some(), Duration::from_secs(2));
        click_button_labeled(&menu.popover_box, "Current chapter");

        // `AdwToastOverlay` shows one toast at a time and queues the rest — "Download failed" is
        // queued behind the "Download started" toast the click itself raised, so this can't land
        // until that first toast's own display duration lapses. Comfortably past a toast's
        // default lifetime, not tuned to race it.
        pump_until(
            || any_label_reads(toast_overlay.upcast_ref(), "Download failed — waiting for a non-metered connection"),
            Duration::from_secs(10),
        );
        assert!(
            any_label_reads(toast_overlay.upcast_ref(), "Download failed — waiting for a non-metered connection"),
            "a failed download must toast the actual reason, not just leave the icon to quietly revert"
        );

        window.destroy();
    }

    /// Finds and clicks the button with the given label anywhere inside a container, so tests can
    /// drive widgets the same way a user tapping them would. The text may sit on the button
    /// itself (the flat scope rows used to) or on a label inside it (scope rows are title +
    /// subtitle stacks now; "Next chapters" carries its title in its child box).
    fn click_button_labeled(container: &gtk4::Box, label: &str) {
        button_labeled(container, label).unwrap_or_else(|| panic!("no button labeled {label:?} found")).emit_clicked();
    }

    /// The button with the given label anywhere inside a container, if it currently exists —
    /// the read half of `click_button_labeled`, also used to poll for a popover's async row
    /// population having landed before clicking (see its own call sites).
    fn button_labeled(container: &gtk4::Box, label: &str) -> Option<gtk4::Button> {
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
        found
    }

    /// A button's effective text: its own label, else the first label inside its child box.
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

    /// Visits every descendant of `root` depth-first.
    fn for_each_descendant(root: &gtk4::Widget, f: &mut dyn FnMut(&gtk4::Widget)) {
        let mut child = root.first_child();
        while let Some(widget) = child {
            f(&widget);
            for_each_descendant(&widget, f);
            child = widget.next_sibling();
        }
    }

    /// Finds the "Next chapters" row's stepper: the `−` button (whose parent box also holds the
    /// count label and the `+` button as its next siblings, per `populate_download_popover_rows`).
    fn stepper_widgets(container: &gtk4::Box) -> (gtk4::Button, gtk4::Label, gtk4::Button) {
        let mut found: Option<(gtk4::Button, gtk4::Label, gtk4::Button)> = None;
        for_each_descendant(container.upcast_ref(), &mut |widget| {
            if found.is_some() {
                return;
            }
            let Some(button) = widget.downcast_ref::<gtk4::Button>() else { return };
            if button.label().as_deref() != Some("−") {
                return;
            }
            let parent = button.parent().expect("stepper button has a parent box").downcast::<gtk4::Box>().unwrap();
            let count = parent.first_child().and_then(|m| m.next_sibling()).expect("count label follows the − button").downcast::<gtk4::Label>().unwrap();
            let plus = count.next_sibling().expect("+ button follows the count label").downcast::<gtk4::Button>().unwrap();
            found = Some((button.clone(), count, plus));
        });
        found.expect("a − button (the Next-chapters stepper) exists in the popover")
    }

    /// Every label's text under `root`, in order — the generic form the specific collectors below
    /// filter.
    fn for_each_descendant_labels(root: &gtk4::Box) -> Vec<String> {
        let mut texts = Vec::new();
        for_each_descendant(root.upcast_ref(), &mut |widget| {
            if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
                texts.push(label.label().to_string());
            }
        });
        texts
    }

    /// The popover's size-estimate subtitles, in row order — the labels starting with the spec's
    /// "≈" (rows without an estimable size simply have no such label).
    /// The book's total size stands in for "Entire book" only, only while the files' own sizes
    /// aren't cached, and never over them.
    #[test]
    fn the_books_total_size_only_stands_in_for_the_whole_book() {
        let sized = [TrackRef { ino: "1".into(), track_index: 0, duration_seconds: 10.0, offset_seconds: 0.0, size_bytes: Some(1_000_000) }];
        let unsized_ = [TrackRef { ino: "1".into(), track_index: 0, duration_seconds: 10.0, offset_seconds: 0.0, size_bytes: None }];
        let chapters = [(0.0, 5.0), (5.0, 10.0)];
        let total = Some(340_000_000);
        // No files known: the whole book is the book's size; nothing else is guessed.
        assert_eq!(estimate_bytes_for(&[], &chapters, DownloadScope::EntireBook, 0, total), Some(340_000_000));
        for scope in [DownloadScope::CurrentChapter, DownloadScope::NextChapters(2), DownloadScope::RemainingChapters] {
            assert_eq!(estimate_bytes_for(&[], &chapters, scope, 0, total), None, "{scope:?}");
        }
        assert_eq!(estimate_bytes_for(&unsized_, &chapters, DownloadScope::EntireBook, 0, total), Some(340_000_000), "files without sizes don't count");
        // The files' own sizes win.
        assert_eq!(estimate_bytes_for(&sized, &chapters, DownloadScope::EntireBook, 0, total), Some(1_000_000));
        // Unknown or zero: no estimate, as before.
        assert_eq!(estimate_bytes_for(&[], &chapters, DownloadScope::EntireBook, 0, None), None);
        assert_eq!(estimate_bytes_for(&[], &chapters, DownloadScope::EntireBook, 0, Some(0)), None);
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A book whose files aren't cached yet but
    /// whose total size the library list gave: "Entire book" shows it, the rest show no estimate.
    pub(crate) fn run_entire_book_shows_the_list_size_before_the_files_are_known(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::downloads::tests::mock_three_track_item(&mock_server, "item-1"));
        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(crate::player::tests::account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(crate::player::tests::insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        runtime
            .block_on(abs_storage::repo::items::set_extras(&pool, &server.id, "item-1", &abs_storage::models::ItemExtras { size_bytes: Some(340_000_000), tags_json: "[]".into(), ..Default::default() }))
            .unwrap();
        assert!(runtime.block_on(abs_core::tracks::cached_tracks(&pool, &server.id, "item-1")).unwrap().is_empty());

        let session = abs_core::auth::Session::new(pool.clone(), &server, &account);
        let chapter_ranges = vec![(0.0, 5.0), (5.0, 10.0), (10.0, 15.0)];
        let menu = build(pool.clone(), test_download_manager(pool.clone()), session, "item-1".to_string(), move || chapter_ranges.clone(), || true, || 0, || None, adw::ToastOverlay::new(), Rc::new(|| {}));
        let window = mapped_window(&menu.widget);
        menu.popover.popup();
        pump_until(|| estimate_texts(&menu.popover_box) == ["≈340.0 MB".to_string()], Duration::from_secs(3));
        assert_eq!(estimate_texts(&menu.popover_box), ["≈340.0 MB"], "only the whole book has an estimate");
        assert!(button_labeled(&menu.popover_box, "Current chapter").is_some(), "the other rows are still there, and usable");
        window.destroy();
    }

    fn estimate_texts(container: &gtk4::Box) -> Vec<String> {
        for_each_descendant_labels(container).into_iter().filter(|text| text.starts_with('≈')).collect()
    }

    /// How many scope rows currently show the spec's "Not enough free space" subtitle.
    fn blocked_row_count(container: &gtk4::Box) -> usize {
        for_each_descendant_labels(container).into_iter().filter(|text| text == "Not enough free space").count()
    }
}
