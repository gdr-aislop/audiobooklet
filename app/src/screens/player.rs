//! The full player screen — `docs/design/ui-spec.md`'s "Player — full" section: core transport,
//! a secondary row of speed/sleep-timer/chapters controls, and a header `⋯` menu for "Mark
//! as finished" and "Reset progress". Opened by `main_window` swapping window
//! content in (there's no `AdwNavigationView`/`AdwDialog` available at this crate's libadwaita
//! ceiling, both v1.4+); the down-chevron header button, the `Esc` key, and a downward swipe
//! anywhere on the screen (mirroring the mini bar's own swipe-up gesture, minus the tap
//! component — see `player_gesture_should_collapse`) all call `on_collapse` to swap back.
//!
//! The keyboard-only equivalents of the on-screen controls (ui-spec §6's full-player set:
//! arrow-key skip, speed stepping, `c`/`t`/Escape) are registered into the `SimpleActionGroup`
//! exposed on `PlayerScreen` — `main_window` merges it under the "player" action prefix for as
//! long as this screen is open and removes it on collapse, so those accelerators are inert
//! everywhere else.

use std::rc::Rc;

use adw::glib;
use adw::prelude::*;

use abs_core::playback::SPEED_PRESETS;

use crate::downloads::DownloadManager;
use crate::i18n::{ntr, tr, tr_args, tr_noop};
use crate::player::{ChapterInfo, PlayerController, PlayerSnapshot};

pub struct PlayerScreen {
    pub root: gtk4::Widget,
    /// The screen's keyboard actions, named per ui-spec §6 (`skip-back`, `skip-forward`,
    /// `speed-up`, `speed-down`, `speed-reset`, `chapters`, `sleep-timer`, `collapse`). The
    /// window merges this group under the "player" prefix while the screen is open — see the
    /// module docs.
    pub actions: gtk4::gio::SimpleActionGroup,
    #[cfg(test)]
    hooks: TestHooks,
}

#[cfg(test)]
pub struct TestHooks {
    pub window_title: adw::WindowTitle,
    pub title_label: gtk4::Label,
    pub author_label: gtk4::Label,
    pub chapter_label: gtk4::Label,
    pub play_button: gtk4::Button,
    pub previous_chapter_button: gtk4::Button,
    pub next_chapter_button: gtk4::Button,
    pub collapse_button: gtk4::Button,
    pub scrubber: gtk4::Scale,
    pub elapsed_label: gtk4::Label,
    pub remaining_label: gtk4::Label,
    pub book_left_label: gtk4::Label,
    pub chapters_button: gtk4::MenuButton,
    pub chapters_popover: gtk4::Popover,
    pub chapters_list: gtk4::ListBox,
    pub speed_button: gtk4::MenuButton,
    pub speed_label: gtk4::Label,
    pub speed_popover_box: gtk4::Box,
    pub sleep_timer_button: gtk4::MenuButton,
    pub sleep_timer_popover: gtk4::Popover,
    pub sleep_timer_popover_box: gtk4::Box,
    pub mark_as_finished_button: gtk4::Button,
    pub reset_progress_button: gtk4::Button,
    pub download_button: gtk4::MenuButton,
    pub download_popover: gtk4::Popover,
    pub download_popover_box: gtk4::Box,
    pub error_banner: crate::widgets::banner::ErrorBanner,
    pub download_progress_revealer: gtk4::Revealer,
    pub cover: crate::widgets::cover_image::CoverImage,
}

#[cfg(test)]
impl PlayerScreen {
    pub fn test_hooks(&self) -> &TestHooks {
        &self.hooks
    }
}

pub fn build(
    pool: sqlx::SqlitePool,
    controller: PlayerController,
    download_manager: DownloadManager,
    on_collapse: impl Fn() + 'static,
    on_open_downloads: impl Fn() + 'static,
) -> PlayerScreen {
    // Shared by the down-chevron header button and the Escape action below.
    let on_collapse = Rc::new(on_collapse);
    // Leaving for the Downloads tab is a way out of the Player like collapsing it: the screen's
    // snapshot hook must not stay on the controller, updating widgets nobody sees.
    let on_open_downloads: Rc<dyn Fn()> = Rc::new({
        let controller = controller.clone();
        move || {
            controller.clear_full_update();
            on_open_downloads();
        }
    });
    let header = adw::HeaderBar::new();
    let collapse_button = gtk4::Button::from_icon_name("go-down-symbolic");
    collapse_button.connect_clicked({
        let controller = controller.clone();
        let on_collapse = on_collapse.clone();
        move |_| {
            controller.clear_full_update();
            on_collapse();
        }
    });
    header.pack_start(&collapse_button);
    // The subtitle says where the audio comes from; see `PlayerSnapshot::source_is_local`.
    let window_title = adw::WindowTitle::new(&tr("Now Playing"), "");
    header.set_title_widget(Some(&window_title));

    // Surfaces `PlayerSnapshot::last_error` — see that field's doc comment for why this exists at
    // all (previously a playback failure was completely invisible: logged, silently paused, done).
    // Placed above `content` (below the header) so it's visible without disturbing the transport
    // controls, the same "banner near the top of the main content" placement `home.rs`/`library.rs`
    // already use for their own sync-failure banners.
    let error_banner = crate::widgets::banner::ErrorBanner::new();
    error_banner.action_button().connect_clicked({
        let controller = controller.clone();
        move |_| controller.play()
    });

    let cover = crate::widgets::cover_image::CoverImage::new(264);
    cover.widget().set_halign(gtk4::Align::Center);
    cover.widget().set_margin_top(14);
    cover.set_on_activate({
        let controller = controller.clone();
        let anchor = cover.widget().clone();
        move |shown, thumbnail| {
            let Some((session, server_id, item_id)) = controller.current_download_context() else { return };
            let title = controller.snapshot().map(|s| s.title).unwrap_or_default();
            crate::widgets::cover_viewer::open(
                &anchor,
                crate::widgets::cover_viewer::CoverSource { session, paths: controller.paths(), server_id, item_id, title, thumbnail, shown },
            );
        }
    });
    // `max_width_chars(1)` caps each label's natural width regardless of `wrap` — this screen's
    // content scrolls only vertically (`hscrollbar_policy(Never)` below), so an unclamped, fully
    // server-controlled title/author string could otherwise force the whole window wider than
    // the screen (see `widgets::banner`'s identical fix for the same reasoning).
    let title_label = gtk4::Label::builder()
        .wrap(true)
        .max_width_chars(1)
        .justify(gtk4::Justification::Center)
        .css_classes(["title-2"])
        .margin_top(16)
        .build();
    let author_label =
        gtk4::Label::builder().wrap(true).max_width_chars(1).justify(gtk4::Justification::Center).css_classes(["dim-label"]).margin_top(4).build();
    // The chapter playing, under the author (Lissen shows it in the author's place). One line,
    // ellipsized, so it never adds more than a line of height on a phone; tapping it opens the
    // chapters list, like the chapters button.
    let chapter_label = gtk4::Label::builder()
        .max_width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(["accent"])
        .tooltip_text(tr("Chapters"))
        .margin_top(6)
        .visible(false)
        .build();

    let scrubber = gtk4::Scale::builder().orientation(gtk4::Orientation::Horizontal).hexpand(true).build();
    scrubber.set_range(0.0, 1.0);
    scrubber.set_draw_value(false);
    let elapsed_label = gtk4::Label::builder().xalign(0.0).css_classes(["caption", "dim-label"]).build();
    let remaining_label = gtk4::Label::builder().xalign(1.0).css_classes(["caption", "dim-label"]).build();
    // With chapters, the scrubber and the two labels above cover the current chapter, so the
    // book's own time left goes in the middle of the row.
    // A `GtkCenterBox` keeps it centred whatever the side labels' widths, at its full natural
    // width; ellipsizing only lets it shrink on a screen too narrow for it.
    let book_left_label = gtk4::Label::builder()
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .margin_start(6)
        .margin_end(6)
        .css_classes(["caption", "dim-label"])
        .visible(false)
        .build();
    let time_row = gtk4::CenterBox::builder().margin_top(8).build();
    time_row.set_start_widget(Some(&elapsed_label));
    time_row.set_center_widget(Some(&book_left_label));
    time_row.set_end_widget(Some(&remaining_label));

    // Previous/next chapter sit at the outer ends of the row, the same buttons the system media
    // card's ⏮/⏭ are (MPRIS `Previous`/`Next`), with the skip-N-seconds pair inside them.
    let previous_chapter = gtk4::Button::builder()
        .css_classes(["circular", "flat"])
        .tooltip_text(tr("Previous chapter"))
        .child(&gtk4::Image::from_icon_name("media-skip-backward-symbolic"))
        .build();
    let skip_back = gtk4::Button::builder()
        .css_classes(["circular"])
        .child(&gtk4::Image::from_icon_name("media-seek-backward-symbolic"))
        .build();
    // A persistent icon, updated in place via `set_icon_name` (see `update` below) rather than a
    // fresh `gtk4::Image::from_icon_name` per snapshot — this screen's `update` closure runs once
    // per player tick (4 times a second while playing), and allocating and swapping in a whole
    // new child widget that often, whether or not the playing state actually changed, forced a
    // needless relayout on every firing. The mini bar's own play icon already gets this right.
    let play_icon = gtk4::Image::from_icon_name("media-playback-pause-symbolic");
    let play_button = gtk4::Button::builder()
        .css_classes(["circular", "suggested-action"])
        .width_request(72)
        .height_request(72)
        .child(&play_icon)
        .build();
    let skip_forward = gtk4::Button::builder()
        .css_classes(["circular"])
        .child(&gtk4::Image::from_icon_name("media-seek-forward-symbolic"))
        .build();
    let next_chapter = gtk4::Button::builder()
        .css_classes(["circular", "flat"])
        .tooltip_text(tr("Next chapter"))
        .child(&gtk4::Image::from_icon_name("media-skip-forward-symbolic"))
        .build();
    // Five buttons have to fit a 360px-wide phone between `content`'s 28px margins, hence the
    // tighter spacing than the three-button row had.
    let transport = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(18).halign(gtk4::Align::Center).margin_top(26).build();
    transport.append(&previous_chapter);
    transport.append(&skip_back);
    transport.append(&play_button);
    transport.append(&skip_forward);
    transport.append(&next_chapter);
    for button in [&previous_chapter, &skip_back, &skip_forward, &next_chapter] {
        button.set_valign(gtk4::Align::Center);
    }

    // Secondary row: speed, sleep timer, chapters. `AdwBottomSheet`/popover-menu widgets from
    // libadwaita 1.4+ are unavailable at this crate's v1.2 ceiling, so every one of these is a
    // plain `GtkMenuButton` + `GtkPopover` holding a `GtkBox` of buttons — matching this
    // codebase's existing convention of wiring everything through direct `connect_clicked`
    // closures rather than `GMenu`/`GAction` models.

    let speed_label = gtk4::Label::new(Some("1.0×")); // i18n: ignore — speed placeholder, replaced by format_speed on the first snapshot
    let speed_popover_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    let speed_popover = gtk4::Popover::builder().child(&speed_popover_box).build();
    let speed_button = gtk4::MenuButton::builder().child(&speed_label).tooltip_text(tr("Playback speed")).popover(&speed_popover).build();
    for speed in SPEED_PRESETS {
        let button = gtk4::Button::builder().label(format_speed(speed)).css_classes(["flat"]).build();
        button.connect_clicked({
            let controller = controller.clone();
            let speed_popover = speed_popover.clone();
            move |_| {
                controller.set_speed(speed);
                speed_popover.popdown();
            }
        });
        speed_popover_box.append(&button);
    }

    let sleep_timer_popover_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    let sleep_timer_popover = gtk4::Popover::builder().child(&sleep_timer_popover_box).build();
    let sleep_timer_button = gtk4::MenuButton::builder()
        .icon_name("weather-clear-night-symbolic")
        .tooltip_text(tr("Sleep timer"))
        .popover(&sleep_timer_popover)
        .build();
    type SleepTimerAction = Box<dyn Fn(&PlayerController)>;
    let sleep_timer_options: [(String, SleepTimerAction); 5] = [
        (tr("Off"), Box::new(|c: &PlayerController| c.cancel_sleep_timer())),
        (ntr("{count} minute", "{count} minutes", 15), Box::new(|c: &PlayerController| c.set_sleep_timer_minutes(15))),
        (ntr("{count} minute", "{count} minutes", 30), Box::new(|c: &PlayerController| c.set_sleep_timer_minutes(30))),
        (ntr("{count} minute", "{count} minutes", 45), Box::new(|c: &PlayerController| c.set_sleep_timer_minutes(45))),
        (tr("End of chapter"), Box::new(|c: &PlayerController| c.set_sleep_timer_end_of_chapter())),
    ];
    for (label, apply) in sleep_timer_options {
        let button = gtk4::Button::builder().label(label).css_classes(["flat"]).build();
        button.connect_clicked({
            let controller = controller.clone();
            let sleep_timer_popover = sleep_timer_popover.clone();
            move |_| {
                apply(&controller);
                sleep_timer_popover.popdown();
            }
        });
        sleep_timer_popover_box.append(&button);
    }

    let chapters_list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    // "downloaded" legend (ui-spec: sits in the Chapters section header so the downloaded
    // glyph's meaning doesn't need to be inferred from a single unlabeled icon on the active
    // chapters) — the same folder-download glyph the chapter rows and the covers' downloaded
    // badge use, so the icon itself is the legend.
    let chapters_legend = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(4).margin_bottom(4).build();
    chapters_legend.append(&gtk4::Image::builder().icon_name("folder-download-symbolic").css_classes(["dim-label"]).build());
    chapters_legend.append(&gtk4::Label::builder().label(tr("downloaded")).css_classes(["caption", "dim-label"]).build());
    let chapters_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    chapters_box.append(&chapters_legend);
    chapters_box.append(&chapters_list);
    let chapters_scroller = gtk4::ScrolledWindow::builder()
        .max_content_height(320)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .width_request(260)
        .child(&chapters_box)
        .build();
    let chapters_popover = gtk4::Popover::builder().child(&chapters_scroller).build();
    let chapters_button = gtk4::MenuButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text(tr("Chapters"))
        .popover(&chapters_popover)
        .build();

    let secondary_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(18).halign(gtk4::Align::Center).margin_top(10).build();
    secondary_row.append(&speed_button);
    secondary_row.append(&sleep_timer_button);
    secondary_row.append(&chapters_button);

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .margin_start(28)
        .margin_end(28)
        .margin_bottom(24)
        .build();
    content.append(cover.widget());
    content.append(&title_label);
    content.append(&author_label);
    content.append(&chapter_label);
    content.append(&scrubber);
    content.append(&time_row);
    content.append(&transport);
    content.append(&secondary_row);

    // Scrolls vertically so the screen can get as short as a phone's: unscrolled, its minimum
    // height (~630px with a one-line title, 800 with a long title and the error banner) is the
    // window's minimum while it's shown, and the window never shrinks back afterwards — which
    // left Home's tab bar under the bottom of the phone's screen. Where everything fits nothing
    // scrolls, and the swipe-down below still collapses the screen.
    let content_scroller =
        gtk4::ScrolledWindow::builder().hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).child(&content).build();

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&header);
    root.append(error_banner.widget());
    root.append(&content_scroller);

    // Swipe-down-to-collapse — the counterpart to the mini bar's own swipe-up-to-open gesture
    // (see `main_window`'s `mini_bar_gesture_should_open`). Added directly on `root`, the same
    // "gesture on an ancestor, button/scale descendants keep working" arrangement already proven
    // there: a click on `collapse_button`/`menu_button` or a drag on `scrubber` claims its own
    // touch sequence, so this ancestor `GestureDrag` never sees a `drag-end` for those.
    let player_gesture = gtk4::GestureDrag::new();
    player_gesture.connect_drag_end({
        let controller = controller.clone();
        let on_collapse = on_collapse.clone();
        move |gesture, _, _| {
            if let Some((offset_x, offset_y)) = gesture.offset() {
                if player_gesture_should_collapse(offset_x, offset_y) {
                    controller.clear_full_update();
                    on_collapse();
                }
            }
        }
    });
    root.add_controller(player_gesture);

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&root));

    // The download button + its scope popover — shared with Item Detail's own
    // (`widgets::download_scope_menu`; see its module doc for why this moved out of this file).
    // `current_download_context()` is read once, here, rather than inside a closure: a
    // `PlayerScreen` is built fresh on every mini-bar tap, so there is always a book loaded or
    // starting by the time this runs — unlike `chapter_ranges`/`current_chapter_index`/free space
    // below, which the widget itself re-asks on every popover open since those genuinely change
    // over a session's lifetime.
    let (download_session, download_server_id, download_item_id) =
        controller.current_download_context().expect("a player screen is only ever built once a book is loaded or starting");
    // Visible progress for this item's own in-flight download (see `widgets::download_progress`'s
    // doc) — built before `download_menu` moves `download_item_id`, appended to `content` below
    // (its last child is `secondary_row`, so a plain `append` here lands right after it).
    let (progress_strip_widget, _progress_strip) =
        crate::widgets::download_progress::DownloadProgressStrip::build(download_manager.clone(), download_server_id.clone(), download_item_id.clone());
    let download_menu = crate::widgets::download_scope_menu::build(
        pool.clone(),
        download_manager.clone(),
        download_session,
        download_item_id,
        {
            let controller = controller.clone();
            move || controller.chapters().iter().map(|c| (c.start_seconds, c.end_seconds)).collect()
        },
        // The Player can open while its book is still starting; its chapters (however many
        // there are — zero for a chapterless book) are known once that is over.
        {
            let controller = controller.clone();
            move || controller.snapshot().is_some_and(|s| !s.is_loading)
        },
        {
            let controller = controller.clone();
            move || controller.current_chapter_index().unwrap_or(0)
        },
        {
            let download_manager = download_manager.clone();
            move || download_manager.free_space_bytes()
        },
        toast_overlay.clone(),
        on_open_downloads.clone(),
    );
    secondary_row.append(&download_menu.widget);
    content.append(&progress_strip_widget);

    // The spec's `⋯` menu is down to two testing/recovery actions — speed and sleep timer live
    // as secondary-row buttons instead. Both are the shared `item_options_menu` widget (also used
    // by Item Detail).
    let options_menu = crate::widgets::item_options_menu::build(
        {
            let controller = controller.clone();
            let toast_overlay = toast_overlay.clone();
            move || {
                let before = controller.snapshot().map(|s| s.position_seconds).unwrap_or(0.0);
                // Only a controller that acted has anything to undo.
                if controller.mark_as_finished() {
                    toast_overlay.add_toast(undo_position_toast(&controller, &tr("Marked as finished"), before));
                }
            }
        },
        {
            let controller = controller.clone();
            let toast_overlay = toast_overlay.clone();
            move || {
                let before = controller.snapshot().map(|s| s.position_seconds).unwrap_or(0.0);
                if controller.reset_progress() {
                    toast_overlay.add_toast(undo_position_toast(&controller, &tr("Progress reset"), before));
                }
            }
        },
    );
    header.pack_end(&options_menu.widget);

    // The skip intervals are read at click time from the controller (Settings → Playback's live
    // config) rather than captured from `playback_settings` at build — a Settings edit applies to
    // the very next skip, here and in the keyboard actions below.
    skip_back.connect_clicked({
        let controller = controller.clone();
        move |_| controller.skip(-controller.skip_intervals().0)
    });
    skip_forward.connect_clicked({
        let controller = controller.clone();
        move |_| controller.skip(controller.skip_intervals().1)
    });
    let chapter_label_click = gtk4::GestureClick::new();
    chapter_label_click.connect_released({
        let chapters_popover = chapters_popover.clone();
        move |_, _, _, _| chapters_popover.popup()
    });
    chapter_label.add_controller(chapter_label_click);
    previous_chapter.connect_clicked({
        let controller = controller.clone();
        move |_| {
            controller.previous_chapter();
        }
    });
    next_chapter.connect_clicked({
        let controller = controller.clone();
        move |_| {
            controller.next_chapter();
        }
    });
    play_button.connect_clicked({
        let controller = controller.clone();
        move |_| controller.toggle_play_pause()
    });

    // Keyboard actions (ui-spec §6's full-player table), registered into the group the main
    // window merges under the "player" prefix. Every action mirrors a button on this screen —
    // the arrow keys reuse the transport buttons' skip intervals, the speed keys step through
    // `SPEED_PRESETS` relative to the current speed, and `c`/`t` pop the same popovers their
    // menu buttons open.
    let actions = gtk4::gio::SimpleActionGroup::new();
    add_action(&actions, "skip-back", { let controller = controller.clone(); move || controller.skip(-controller.skip_intervals().0) });
    add_action(&actions, "skip-forward", { let controller = controller.clone(); move || controller.skip(controller.skip_intervals().1) });

    // Speed steps are relative to whatever is current, so they read as "next/previous preset" no
    // matter where in the list the user is — including from a speed that came straight from the
    // Settings default and isn't itself a preset. At either end of the list the step is a no-op.
    let step_speed = {
        let controller = controller.clone();
        move |up: bool| {
            let current = controller.snapshot().map(|s| s.speed).unwrap_or(1.0);
            let target = if up {
                SPEED_PRESETS.iter().copied().find(|p| *p > current + f64::EPSILON)
            } else {
                SPEED_PRESETS.iter().copied().rev().find(|p| *p < current - f64::EPSILON)
            };
            if let Some(speed) = target {
                controller.set_speed(speed);
            }
        }
    };
    add_action(&actions, "speed-up", { let step_speed = step_speed.clone(); move || step_speed(true) });
    add_action(&actions, "speed-down", move || step_speed(false));
    add_action(&actions, "speed-reset", {
        let controller = controller.clone();
        move || {
            // Guarding the already-at-1× case skips `set_speed`'s redundant seek-with-rate (the
            // same reasoning `start()` applies before re-applying the default speed).
            if controller.snapshot().is_some_and(|s| (s.speed - 1.0).abs() > f64::EPSILON) {
                controller.set_speed(1.0);
            }
        }
    });
    add_action(&actions, "chapters", { let popover = chapters_popover.clone(); move || popover.popup() });
    add_action(&actions, "sleep-timer", { let popover = sleep_timer_popover.clone(); move || popover.popup() });
    add_action(&actions, "collapse", {
        let controller = controller.clone();
        let on_collapse = on_collapse.clone();
        move || {
            // Escape over the cover viewer closes the viewer, not the Player under it.
            if crate::widgets::cover_viewer::close_current() {
                return;
            }
            controller.clear_full_update();
            on_collapse();
        }
    });

    // Populated fresh every time the popover is about to open (not once at screen-build time), so
    // "current chapter highlighted" always reflects the position at the moment it's opened.
    chapters_popover.connect_show({
        let controller = controller.clone();
        let chapters_list = chapters_list.clone();
        let pool = pool.clone();
        move |_| {
            let chapters = controller.chapters();
            let position = controller.snapshot().map(|s| s.position_seconds).unwrap_or(0.0);
            rebuild_chapters_list(&chapters_list, &chapters, position, &[]);

            // Offline markers need an async DB read (`complete_inos_for_item`), unlike everything
            // else here (in-memory on `controller`) — render without them first, then refine once
            // the query lands, same "show now, refine once the async bit lands" shape `home.rs`'s
            // cover-art re-render already uses.
            if let Some((_, server_id, item_id)) = controller.current_download_context() {
                let pool = pool.clone();
                let chapters_list = chapters_list.clone();
                let controller = controller.clone();
                glib::spawn_future_local(async move {
                    let chapters = controller.chapters();
                    if chapters.is_empty() {
                        return;
                    }
                    let chapter_ranges: Vec<(f64, f64)> = chapters.iter().map(|c| (c.start_seconds, c.end_seconds)).collect();
                    let markers = abs_core::download_tracks::chapter_offline_markers_for_item(&pool, &server_id, &item_id, &chapter_ranges).await.unwrap_or_default();
                    let position = controller.snapshot().map(|s| s.position_seconds).unwrap_or(0.0);
                    rebuild_chapters_list(&chapters_list, &chapters, position, &markers);
                });
            }
        }
    });
    chapters_list.connect_row_activated({
        let controller = controller.clone();
        let chapters_popover = chapters_popover.clone();
        move |_, row| {
            if let Some(chapter) = controller.chapters().get(row.index() as usize) {
                controller.seek_to_seconds(chapter.start_seconds);
            }
            chapters_popover.popdown();
        }
    });

    // `value-changed` fires both for user drags and for our own `scrubber.set_value(...)` calls
    // from `update()` below — a `RefCell` guard distinguishes the two so a live position update
    // doesn't get immediately re-interpreted as the user dragging (which would fight the real
    // playback position every tick).
    let updating_from_snapshot = std::rc::Rc::new(std::cell::Cell::new(false));
    // A drag moves the scale through dozens of values; seeking at each one meant a flushing seek
    // (on a stream, a new range request, and across files a whole track load) per pixel. Only
    // the value the knob settles on is sought, once it has been still for `SCRUB_SETTLE` —
    // whatever moved it (a drag, a tap, a key, a scroll). Meanwhile the time labels follow the
    // knob, and the snapshot doesn't move it back under the finger.
    let pending_scrub: Rc<std::cell::RefCell<Option<glib::SourceId>>> = Rc::new(std::cell::RefCell::new(None));
    // The book-level span the scrubber covers (see `scrub_range`). Like the knob, it's only
    // updated while nothing is being scrubbed, so a drag that playback carries across a chapter
    // boundary keeps mapping onto the chapter it started in.
    let scrub_span: Rc<std::cell::Cell<(f64, f64)>> = Rc::new(std::cell::Cell::new((0.0, 0.0)));
    scrubber.connect_value_changed({
        let controller = controller.clone();
        let updating_from_snapshot = updating_from_snapshot.clone();
        let pending_scrub = pending_scrub.clone();
        let scrub_span = scrub_span.clone();
        let elapsed_label = elapsed_label.clone();
        let remaining_label = remaining_label.clone();
        move |scale| {
            if updating_from_snapshot.get() {
                return;
            }
            let fraction = scale.value();
            let (start, end) = scrub_span.get();
            if end <= start {
                return;
            }
            set_time_labels(&elapsed_label, &remaining_label, fraction * (end - start), end - start);
            if let Some(source) = pending_scrub.borrow_mut().take() {
                source.remove();
            }
            let source = glib::timeout_add_local_once(SCRUB_SETTLE, {
                let controller = controller.clone();
                let pending_scrub = pending_scrub.clone();
                move || {
                    // Fired: the source is gone, so nothing may `remove()` it any more.
                    pending_scrub.borrow_mut().take();
                    tracing::info!(fraction, start, end, "scrub: seeking");
                    controller.seek_to_seconds(start + fraction * (end - start));
                }
            });
            *pending_scrub.borrow_mut() = Some(source);
        }
    });

    let update = {
        let window_title = window_title.clone();
        let title_label = title_label.clone();
        let author_label = author_label.clone();
        let chapter_label = chapter_label.clone();
        let play_icon = play_icon.clone();
        let scrubber = scrubber.clone();
        let elapsed_label = elapsed_label.clone();
        let remaining_label = remaining_label.clone();
        let book_left_label = book_left_label.clone();
        let scrub_span = scrub_span.clone();
        let speed_label = speed_label.clone();
        let sleep_timer_button = sleep_timer_button.clone();
        let skip_back = skip_back.clone();
        let skip_forward = skip_forward.clone();
        let previous_chapter = previous_chapter.clone();
        let next_chapter = next_chapter.clone();
        let speed_button = speed_button.clone();
        let cover = cover.clone();
        let error_banner = error_banner.clone();
        move |snapshot: &PlayerSnapshot| {
            // Unknown until a book has loaded. `update` runs 4 times a second, so only touch the
            // subtitle when it changes.
            let source = match (snapshot.is_loading, snapshot.source_is_local) {
                (true, _) => String::new(),
                (false, true) => tr("On this device"),
                (false, false) => tr("Streaming"),
            };
            if window_title.subtitle() != source {
                window_title.set_subtitle(&source);
            }
            title_label.set_label(&snapshot.title);
            author_label.set_label(snapshot.author.as_deref().unwrap_or(""));
            author_label.set_visible(snapshot.author.is_some());
            match &snapshot.chapter {
                Some(chapter) => {
                    // TRANSLATORS: the chapter line under the author, e.g. "Chapter 3 · 3 of 12". {title} is the chapter's name, {number} its 1-based position and {total} how many chapters the book has.
                    let text = tr_args(
                        "{title} · {number} of {total}",
                        &[("title", &chapter.title), ("number", &chapter.number.to_string()), ("total", &chapter.count.to_string())],
                    );
                    // `update` runs 4 times a second; only touch the label when the chapter changes.
                    if chapter_label.label() != text {
                        chapter_label.set_label(&text);
                    }
                    chapter_label.set_visible(true);
                }
                None => chapter_label.set_visible(false),
            }
            cover.set_path(snapshot.cover_path.as_deref());
            play_icon.set_icon_name(Some(if snapshot.shows_pause_button() {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            }));

            // While the listener is still moving the knob, it, its span and the time labels are
            // theirs.
            if pending_scrub.borrow().is_none() {
                let (start, end) = scrub_range(snapshot);
                scrub_span.set((start, end));
                let fraction = if end > start { ((snapshot.position_seconds - start) / (end - start)).clamp(0.0, 1.0) } else { 0.0 };
                updating_from_snapshot.set(true);
                scrubber.set_value(fraction);
                updating_from_snapshot.set(false);
                let into = (snapshot.position_seconds - start).clamp(0.0, (end - start).max(0.0));
                set_time_labels(&elapsed_label, &remaining_label, into, end - start);
            }
            // A book that hasn't loaded (or failed to start) has no length yet: "0:00 / -0:00"
            // would just be noise. Hidden by opacity, so the row keeps its height.
            let length_known = snapshot.duration_seconds > 0.0;
            for label in [&elapsed_label, &remaining_label] {
                label.set_opacity(if length_known { 1.0 } else { 0.0 });
            }
            if snapshot.chapter.is_some() {
                // TRANSLATORS: {time} is the time left in the whole book, e.g. "1:05:09".
                let left = tr_args("{time} left in book", &[("time", &format_hms((snapshot.duration_seconds - snapshot.position_seconds).max(0.0)))]);
                if book_left_label.label() != left {
                    book_left_label.set_label(&left);
                }
                book_left_label.set_visible(true);
            } else {
                book_left_label.set_visible(false);
            }
            // Nothing to seek in, skip over, speed up or time until the book has loaded — each
            // of these would be a no-op the controller swallows, which reads as a dead button.
            scrubber.set_sensitive(!snapshot.is_loading && length_known);
            skip_back.set_sensitive(!snapshot.is_loading);
            skip_forward.set_sensitive(!snapshot.is_loading);
            // Kept while loading (chapters aren't known yet) so the row doesn't reflow when they
            // turn up; only a loaded book without chapters drops them.
            for button in [&previous_chapter, &next_chapter] {
                button.set_visible(snapshot.is_loading || snapshot.has_chapters);
                button.set_sensitive(!snapshot.is_loading);
            }
            speed_button.set_sensitive(!snapshot.is_loading);
            sleep_timer_button.set_sensitive(!snapshot.is_loading);

            speed_label.set_label(&format_speed(snapshot.speed));
            if snapshot.sleep_timer_active {
                sleep_timer_button.add_css_class("accent");
            } else {
                sleep_timer_button.remove_css_class("accent");
            }

            match &snapshot.last_error {
                Some(err) => {
                    let message = friendly_message(err.kind);
                    error_banner.set_title(&tr(message.headline));
                    error_banner.set_description(Some(&tr(message.description)));
                    error_banner.set_action_label(message.action.map(tr).as_deref());
                    error_banner.set_details(Some(err.debug.as_deref().unwrap_or(&err.message)));
                    error_banner.set_revealed(true);
                }
                None => error_banner.set_revealed(false),
            }
        }
    };

    // Paint whatever's already playing immediately, rather than waiting for the next tick.
    if let Some(snapshot) = controller.snapshot() {
        update(&snapshot);
    }
    controller.set_full_update(update);

    PlayerScreen {
        root: toast_overlay.clone().upcast(),
        actions,
        #[cfg(test)]
        hooks: TestHooks {
            cover: cover.clone(),
            window_title,
            title_label,
            author_label,
            chapter_label,
            play_button,
            previous_chapter_button: previous_chapter,
            next_chapter_button: next_chapter,
            collapse_button,
            scrubber,
            elapsed_label,
            remaining_label,
            book_left_label,
            chapters_button,
            chapters_popover,
            chapters_list,
            speed_button,
            speed_label,
            speed_popover_box,
            sleep_timer_button,
            sleep_timer_popover,
            sleep_timer_popover_box,
            #[cfg(test)]
            mark_as_finished_button: options_menu.mark_as_finished_button,
            #[cfg(test)]
            reset_progress_button: options_menu.reset_progress_button,
            #[cfg(test)]
            download_button: download_menu.widget,
            #[cfg(test)]
            download_popover: download_menu.popover,
            #[cfg(test)]
            download_popover_box: download_menu.popover_box,
            #[cfg(test)]
            error_banner,
            #[cfg(test)]
            download_progress_revealer: progress_strip_widget,
        },
    }
}

/// Registers one of the full player's keyboard actions (ui-spec §6) as a stateless
/// `SimpleAction` on the screen's action group, calling `run` on every activation.
fn add_action(group: &gtk4::gio::SimpleActionGroup, name: &'static str, run: impl Fn() + 'static) {
    let action = gtk4::gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| run());
    group.add_action(&action);
}

/// Clears and repopulates `chapters_list`. `markers[i]` (if present for index `i`) shows a small
/// offline glyph trailing chapter `i`'s time — an empty slice (the synchronous first pass, before
/// the async offline lookup lands) means "not downloaded" for every chapter, never "unknown"; a
/// glyph appearing a moment later is the expected two-stage render, not a flicker to hide.
fn rebuild_chapters_list(chapters_list: &gtk4::ListBox, chapters: &[ChapterInfo], position: f64, markers: &[bool]) {
    while let Some(child) = chapters_list.first_child() {
        chapters_list.remove(&child);
    }
    for (index, chapter) in chapters.iter().enumerate() {
        let is_downloaded = markers.get(index).copied().unwrap_or(false);
        chapters_list.append(&build_chapter_row(chapter, position, is_downloaded));
    }
}

/// One row in the chapters sheet: title on the left, start time (plus a small offline glyph when
/// `is_downloaded`) on the right, highlighted (via a css class) if `position` currently falls
/// within this chapter's range.
fn build_chapter_row(chapter: &ChapterInfo, position: f64, is_downloaded: bool) -> gtk4::ListBoxRow {
    let is_current = chapter.start_seconds <= position && position < chapter.end_seconds;

    let title_label = gtk4::Label::builder().label(&chapter.title).xalign(0.0).hexpand(true).ellipsize(gtk4::pango::EllipsizeMode::End).build();
    let time_label = gtk4::Label::builder().label(format_hms(chapter.start_seconds)).css_classes(["dim-label"]).build();
    let row_box = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(12).margin_top(6).margin_bottom(6).build();
    row_box.append(&title_label);
    row_box.append(&time_label);
    if is_downloaded {
        // Same glyph as the covers' downloaded badge — a "this is on disk and playable offline"
        // marker, not a completion checkmark (which is the download *button's* state icon).
        row_box.append(&gtk4::Image::builder().icon_name("folder-download-symbolic").css_classes(["dim-label"]).tooltip_text(tr("Downloaded")).build());
    }

    if is_current {
        title_label.add_css_class("heading");
    }

    gtk4::ListBoxRow::builder().child(&row_box).build()
}

/// Formats a playback speed for the speed button/popover — trims a trailing `.0` (`"1×"` reads
/// oddly for a speed control; `"1.0×"` is the convention every audiobook app uses) but keeps
/// fractional speeds like `1.25×` intact. Shared with Settings' "Default speed" row, which lists
/// the same presets.
pub(crate) fn format_speed(speed: f64) -> String {
    if (speed.fract()).abs() < f64::EPSILON {
        format!("{speed:.1}×")
    } else {
        format!("{speed}×")
    }
}

/// A toast for an action that threw the listening position away (Mark as finished, Reset
/// progress), with an Undo that returns playback to `before` and saves it back — local and
/// server — as unfinished.
fn undo_position_toast(controller: &crate::player::PlayerController, title: &str, before: f64) -> adw::Toast {
    let toast = adw::Toast::builder().title(title).button_label(tr("Undo")).timeout(10).build();
    let controller = controller.clone();
    // The Undo belongs to the book it was offered for; another book may be loaded by the time
    // it's tapped.
    let item_id = controller.current_item_id();
    toast.connect_button_clicked(move |_| {
        if item_id.is_none() || controller.current_item_id() != item_id {
            tracing::info!("undo ignored: a different book is loaded now");
            return;
        }
        controller.seek_to_seconds(before);
        controller.save_progress_now();
    });
    toast
}

pub(crate) fn format_hms(total_seconds: f64) -> String {
    let total_seconds = total_seconds.max(0.0) as u64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Picks a friendly headline and, where retrying is plausibly useful, a "Retry" action label for
/// a [`abs_player::PlaybackErrorKind`]. The raw underlying text (`PlaybackError::message`/
/// `debug`) is never derived from this — it always goes into the banner's "Show details" verbatim
/// (see this screen's `update` closure), so a friendly headline here is never the *only* thing a
/// self-hosting user debugging their server/codec setup can see.
pub(crate) fn friendly_message(kind: abs_player::PlaybackErrorKind) -> FriendlyMessage {
    use abs_player::PlaybackErrorKind::*;
    let (headline, description, action) = match kind {
        MissingCodec => (tr_noop("Can't play this file"), tr_noop("Support for its audio format is missing on this device."), None),
        UnsupportedOrCorrupt => (tr_noop("Can't decode this file"), tr_noop("It may be corrupted, or in a format this app doesn't support."), None),
        ResourceNotFound => (tr_noop("Audio not found on the server"), tr_noop("It may have been moved or deleted."), None),
        NotAuthorized => (tr_noop("The server refused this request"), tr_noop("Try signing in again."), None),
        AudioOutput => (tr_noop("Can't reach the audio output"), tr_noop("Check this device's sound settings and try again."), Some(tr_noop("Retry"))),
        Network => (tr_noop("Connection lost"), tr_noop("Check your connection and try again."), Some(tr_noop("Retry"))),
        Unreachable => (tr_noop("Can't reach your server"), tr_noop("This book can't start until it's back. Check your connection and try again."), Some(tr_noop("Retry"))),
        Seek => (tr_noop("Couldn't get to that position"), tr_noop("Try again, or try another position."), Some(tr_noop("Retry"))),
        Offline => (tr_noop("This part isn't downloaded"), tr_noop("Turn off offline mode to stream it."), None),
        NoAudio => (tr_noop("Nothing to play here"), tr_noop("This item has no audio the app can play, such as a podcast or an e-book."), None),
        Unavailable => (tr_noop("Playback isn't available"), tr_noop("No audio engine could be started on this device."), None),
        Other => (tr_noop("Playback stopped unexpectedly"), tr_noop("Try again."), Some(tr_noop("Retry"))),
    };
    FriendlyMessage { headline, description, action }
}

/// What the player's error banner says: a short headline, a line on what to do, and the
/// action, if retrying is plausibly useful.
pub(crate) struct FriendlyMessage {
    pub headline: &'static str,
    pub description: &'static str,
    pub action: Option<&'static str>,
}

/// A few words for the shell's toast — at most 20 characters, so the toast never cuts them off
/// on a 360px phone next to its "View" and close buttons; the full message is in the player's
/// banner, one tap (tr_noop("View")) away.
pub(crate) fn short_message(kind: abs_player::PlaybackErrorKind) -> &'static str {
    use abs_player::PlaybackErrorKind::*;
    match kind {
        MissingCodec => tr_noop("Can't play this file"),
        UnsupportedOrCorrupt => tr_noop("Can't decode this file"),
        ResourceNotFound => tr_noop("Audio not found"),
        NotAuthorized => tr_noop("Playback refused"),
        AudioOutput => tr_noop("No audio output"),
        Network => tr_noop("Connection lost"),
        Unreachable => tr_noop("Can't reach server"),
        Seek => tr_noop("Couldn't seek"),
        Offline => tr_noop("Not downloaded"),
        NoAudio => tr_noop("No audio"),
        Unavailable => tr_noop("Playback unavailable"),
        Other => tr_noop("Playback stopped"),
    }
}

/// How long the scrubber must rest on a value before it is sought — see the scrubber's
/// `value-changed` handler.
const SCRUB_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

/// The elapsed and remaining labels for `position` of `duration`, both book-level seconds.
/// The book-level span the full player's scrubber covers: the current chapter when the book has
/// chapters, otherwise the whole book.
fn scrub_range(snapshot: &PlayerSnapshot) -> (f64, f64) {
    match &snapshot.chapter {
        Some(chapter) if chapter.end_seconds > chapter.start_seconds => (chapter.start_seconds, chapter.end_seconds),
        _ => (0.0, snapshot.duration_seconds),
    }
}

fn set_time_labels(elapsed_label: &gtk4::Label, remaining_label: &gtk4::Label, position: f64, duration: f64) {
    elapsed_label.set_label(&format_hms(position));
    remaining_label.set_label(&format!("-{}", format_hms((duration - position).max(0.0)))); // i18n: ignore — "-" plus a clock time, not words
}

/// Below this, a downward drag is ignored; at or above it, a predominantly-downward drag
/// collapses the screen back to the mini bar. Unlike the mini bar's own swipe-up gesture (which
/// also treats a tap as "open"), there is deliberately no tap component here — a plain tap on the
/// full player's background (cover, title, empty space) must not collapse it. First-pass
/// calibration only, same as `main_window::SWIPE_UP_MIN_DISTANCE_PX`: the ui-spec flags this as
/// "not yet validated" against real hardware.
const SWIPE_DOWN_MIN_DISTANCE_PX: f64 = 24.0;

/// Whether a completed drag on the full player should collapse it — a predominantly-downward
/// drag that traveled at least `SWIPE_DOWN_MIN_DISTANCE_PX`. `offset_x`/`offset_y` are
/// `GestureDrag::offset()`'s values (total displacement from press to release); a mostly
/// horizontal drag, an upward swipe, or a drag short of the threshold does nothing.
pub(crate) fn player_gesture_should_collapse(offset_x: f64, offset_y: f64) -> bool {
    offset_y >= SWIPE_DOWN_MIN_DISTANCE_PX && offset_y.abs() > offset_x.abs()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::player::tests::{account_and_server, insert_synced_item, mock_playable_item, test_backend};
    use crate::player::PlayRequest;
    use crate::test_support::pump_until;
    use std::time::Duration;

    fn test_download_manager(pool: sqlx::SqlitePool) -> DownloadManager {
        DownloadManager::new(pool, crate::test_support::test_paths(), Box::new(abs_player::network_watch::UnknownNetworkMonitor), false)
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point. Starts real playback
    /// (reusing `player.rs`'s own wiremock-served-audio test helpers), builds the full player
    /// screen against the already-playing controller, and checks its widgets reflect real state.
    pub(crate) fn run(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: Some("Some Author".to_string()) },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));

        let collapsed = std::rc::Rc::new(std::cell::Cell::new(false));
        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), {
            let collapsed = collapsed.clone();
            move || collapsed.set(true)
        }, || {});
        let hooks = screen.test_hooks();

        // The screen paints from `controller.snapshot()` immediately on build, before any tick.
        assert_eq!(hooks.title_label.label(), "Test Book");
        assert_eq!(hooks.author_label.label(), "Some Author");
        assert_eq!(hooks.window_title.subtitle(), "Streaming", "nothing is downloaded, so the header says the book streams");

        // Let a tick or two land so the scrubber/labels reflect a live position.
        pump_until(|| hooks.scrubber.value() >= 0.0, Duration::from_millis(500));
        assert!(!hooks.elapsed_label.label().is_empty());
        assert!(hooks.remaining_label.label().starts_with('-'), "remaining time should count down");

        hooks.play_button.emit_clicked();
        pump_until(|| !controller.snapshot().unwrap().is_playing, Duration::from_secs(5));
        assert!(!controller.snapshot().unwrap().is_playing, "the full player's play/pause button should control the real controller");

        hooks.collapse_button.emit_clicked();
        assert!(collapsed.get(), "the down-chevron should call on_collapse");
        controller.stop();
    }

    /// End-to-end: a real `GstBackend` pipeline fetching from a real (wiremock) HTTP server that
    /// 404s the track file — not a synthetic `PlaybackError` constructed by hand — so this proves
    /// the whole chain (`GstBackend::poll_event`'s classification, `Inner::tick`'s error arm,
    /// `PlayerSnapshot::last_error`, and this screen's banner wiring) actually works together, the
    /// same "real pipeline, not backdoored" convention this file's other test above already uses.
    pub(crate) fn run_playback_error_shows_the_banner(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(async {
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/items/item-1"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "media": { "audioFiles": [{ "ino": "1", "duration": 5.0 }] }
                })))
                .mount(&mock_server)
                .await;
            // The file itself 404s — metadata resolves fine (so `now_playing` has a real
            // title/duration to show), but the actual audio fetch fails once GStreamer's
            // `souphttpsrc` tries to read it.
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/items/item-1/file/1"))
                .respond_with(wiremock::ResponseTemplate::new(404))
                .mount(&mock_server)
                .await;
        });

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();

        pump_until(|| controller.snapshot().is_some_and(|s| s.last_error.is_some()), Duration::from_secs(10));
        assert!(hooks.error_banner.widget().reveals_child(), "a real playback failure must reveal the error banner");
        assert!(!hooks.error_banner.title().is_empty(), "the banner must show a friendly message");
        assert!(hooks.error_banner.details_visible(), "the raw error must be shown behind 'Show details' — never hidden entirely");
        assert!(!hooks.error_banner.details_text().is_empty());

        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Seeds a two-chapter item, opens the
    /// chapters popover, checks both rows are listed with the right titles, then activates the
    /// second row and checks the controller actually seeks to its start.
    pub(crate) fn run_chapters_sheet_lists_and_seeks(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(
            &mock_server,
            "item-1",
            10,
            &[("Intro", 0.0, 4.0), ("Chapter One", 4.0, 10.0)],
        ));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        // Tap-to-seek needs real, seekable audio, not just a parsed `get_item_playback_info`
        // response — hence `mock_playable_item_with_chapters` and the readiness wait below.
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Chaptered Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| !controller.chapters().is_empty(), Duration::from_secs(10));
        // Give the pipeline a moment to actually become seekable (same "position() becomes Some"
        // readiness signal `PlayerController::start` itself waits on for the resume-seek).
        pump_until(|| controller.snapshot().unwrap().position_seconds > 0.0, Duration::from_secs(5));

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();
        assert_eq!(hooks.chapters_button.popover().as_ref(), Some(&hooks.chapters_popover), "the chapters button should open the chapters popover");

        // A `GtkPopover` needs a realized (mapped) toplevel ancestor to actually pop up — unlike
        // every other widget this test suite checks, which only need to exist, not be shown.
        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        hooks.chapters_popover.popup();
        pump_until(|| hooks.chapters_list.row_at_index(1).is_some(), Duration::from_secs(2));

        let row0 = hooks.chapters_list.row_at_index(0).expect("first chapter row");
        let row1 = hooks.chapters_list.row_at_index(1).expect("second chapter row");
        assert!(hooks.chapters_list.row_at_index(2).is_none(), "should list exactly the two chapters");
        assert_eq!(row_title(&row0), "Intro");
        assert_eq!(row_title(&row1), "Chapter One");

        hooks.chapters_list.emit_by_name::<()>("row-activated", &[&row1]);
        pump_until(|| controller.snapshot().unwrap().position_seconds >= 4.0, Duration::from_secs(5));
        assert!(
            controller.snapshot().unwrap().position_seconds >= 4.0,
            "activating the second chapter's row should seek to its start"
        );

        window.destroy();
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The transport row's ⏭ goes to the
    /// next chapter's start and ⏮ (pressed right at a chapter's start) back to the previous one;
    /// MPRIS `Next`/`Previous` (the system media card's ⏮/⏭) do the same.
    pub(crate) fn run_chapter_buttons_jump_between_chapters(runtime: &tokio::runtime::Runtime) {
        use abs_player::mpris::MprisCommands;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(
            &mock_server,
            "item-1",
            10,
            &[("Intro", 0.0, 4.0), ("Chapter One", 4.0, 10.0)],
        ));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Chaptered Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| !controller.chapters().is_empty(), Duration::from_secs(10));
        pump_until(|| controller.snapshot().unwrap().position_seconds > 0.0, Duration::from_secs(5));

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();
        let position = || controller.snapshot().unwrap().position_seconds;
        assert!(hooks.previous_chapter_button.is_visible() && hooks.next_chapter_button.is_visible(), "a chaptered book shows both chapter buttons");
        assert!(hooks.next_chapter_button.is_sensitive(), "the chapter buttons work once the book has loaded");
        assert!(hooks.chapter_label.is_visible(), "a chaptered book shows the chapter line");
        assert_eq!(hooks.chapter_label.label(), "Intro · 1 of 2");

        hooks.next_chapter_button.emit_clicked();
        pump_until(|| position() >= 4.0, Duration::from_secs(5));
        assert!(position() >= 4.0, "next chapter should seek to Chapter One's start");
        pump_until(|| hooks.chapter_label.label() == "Chapter One · 2 of 2", Duration::from_secs(2));
        assert_eq!(hooks.chapter_label.label(), "Chapter One · 2 of 2", "the chapter line follows the position");

        hooks.previous_chapter_button.emit_clicked();
        pump_until(|| position() < 4.0, Duration::from_secs(5));
        assert!(position() < 4.0, "previous chapter at Chapter One's start should go back to the Intro");
        pump_until(|| hooks.chapter_label.label() == "Intro · 1 of 2", Duration::from_secs(2));
        assert_eq!(hooks.chapter_label.label(), "Intro · 1 of 2");

        let mpris = crate::player::MprisBridge::new(controller.clone());
        mpris.next();
        pump_until(|| position() >= 4.0, Duration::from_secs(5));
        assert!(position() >= 4.0, "MPRIS Next should go to the next chapter");
        mpris.previous();
        pump_until(|| position() < 4.0, Duration::from_secs(5));
        assert!(position() < 4.0, "MPRIS Previous should go back a chapter");

        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. A book without chapters hides the
    /// chapter buttons, and MPRIS `Next`/`Previous` fall back to the skip intervals so the
    /// system media card's ⏮/⏭ still do something.
    pub(crate) fn run_chapter_buttons_hide_for_a_book_without_chapters(runtime: &tokio::runtime::Runtime) {
        use abs_player::mpris::MprisCommands;

        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(&mock_server, "item-1", 20, &[]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.set_playback_config(1.0, 2.0, 5.0);
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Plain Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));
        pump_until(|| controller.snapshot().unwrap().position_seconds > 0.0, Duration::from_secs(5));
        controller.pause();

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();
        assert!(!hooks.previous_chapter_button.is_visible() && !hooks.next_chapter_button.is_visible(), "a book without chapters hides the chapter buttons");
        assert!(!hooks.chapter_label.is_visible(), "a book without chapters has no chapter line");
        assert!(!hooks.book_left_label.is_visible(), "without chapters the bar is the whole book, so no separate book time");

        let position = || controller.snapshot().unwrap().position_seconds;
        let before = position();
        crate::player::MprisBridge::new(controller.clone()).next();
        pump_until(|| position() >= before + 4.0, Duration::from_secs(5));
        assert!(position() >= before + 4.0, "MPRIS Next without chapters should skip forward by the 5s interval, got {} from {before}", position());

        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. With chapters, the scrubber and its
    /// time labels cover the current chapter (a drag seeks inside it), and the book's own time
    /// left sits between the labels.
    pub(crate) fn run_scrubber_tracks_the_current_chapter(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(
            &mock_server,
            "item-1",
            10,
            &[("Intro", 0.0, 4.0), ("Chapter One", 4.0, 10.0)],
        ));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Chaptered Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| !controller.chapters().is_empty(), Duration::from_secs(10));
        pump_until(|| controller.snapshot().unwrap().position_seconds > 0.0, Duration::from_secs(5));
        controller.pause();

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();
        let position = || controller.snapshot().unwrap().position_seconds;

        hooks.next_chapter_button.emit_clicked();
        pump_until(|| position() >= 4.0 && hooks.elapsed_label.label() == "0:00", Duration::from_secs(5));
        assert_eq!(hooks.elapsed_label.label(), "0:00", "the time counts from the chapter's start");
        assert_eq!(hooks.remaining_label.label(), "-0:06", "and down to the chapter's end");
        assert!(hooks.scrubber.value() < 0.05, "the knob starts again at the left, got {}", hooks.scrubber.value());
        assert!(hooks.book_left_label.is_visible());
        assert_eq!(hooks.book_left_label.label(), "0:06 left in book");

        hooks.scrubber.set_value(0.5);
        assert_eq!(hooks.elapsed_label.label(), "0:03", "the time follows the knob within the chapter");
        pump_until(|| (position() - 7.0).abs() < 0.5, Duration::from_secs(5));
        assert!((position() - 7.0).abs() < 0.5, "halfway along the bar is halfway through Chapter One (7s), got {}", position());

        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Tapping "Current chapter" in the
    /// download popover should call through to a real download (via `DownloadManager`) for that
    /// chapter's track only, and the button's icon should reflect Downloading -> Complete as the
    /// real `DownloadEvent`s land.
    pub(crate) fn run_download_button_starts_a_download_and_reflects_state(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(&mock_server, "item-1", 10, &[("Intro", 0.0, 4.0), ("Chapter One", 4.0, 10.0)]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Chaptered Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| !controller.chapters().is_empty(), Duration::from_secs(10));

        let download_manager = test_download_manager(pool.clone());
        let screen = build(pool.clone(), controller.clone(), download_manager, || {}, || {});
        let hooks = screen.test_hooks();

        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));

        hooks.download_popover.popup();
        pump_until(|| hooks.download_popover_box.first_child().is_some(), Duration::from_secs(2));
        click_button_labeled(&hooks.download_popover_box, "Current chapter");

        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );
        pump_until(|| hooks.download_button.icon_name().as_deref() == Some("object-select-symbolic"), Duration::from_secs(5));
        assert_eq!(hooks.download_button.icon_name().as_deref(), Some("object-select-symbolic"), "the button should reflect Complete once the download finishes");

        window.destroy();
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The Full Player's own progress strip
    /// (same widget Item Detail uses — see `widgets::download_progress`) must reveal for its own
    /// in-flight download and retract once it completes, exactly like the download button's icon
    /// does, since both are driven off the same `DownloadManager` events.
    pub(crate) fn run_download_progress_strip_reveals_while_downloading(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(&mock_server, "item-1", 10, &[("Intro", 0.0, 4.0), ("Chapter One", 4.0, 10.0)]));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Chaptered Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| !controller.chapters().is_empty(), Duration::from_secs(10));

        let download_manager = test_download_manager(pool.clone());
        let screen = build(pool.clone(), controller.clone(), download_manager, || {}, || {});
        let hooks = screen.test_hooks();

        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));
        assert!(!hooks.download_progress_revealer.reveals_child(), "nothing is downloading yet");

        hooks.download_popover.popup();
        pump_until(|| hooks.download_popover_box.first_child().is_some(), Duration::from_secs(2));
        click_button_labeled(&hooks.download_popover_box, "Current chapter");

        pump_until(|| hooks.download_progress_revealer.reveals_child(), Duration::from_secs(5));
        assert!(hooks.download_progress_revealer.reveals_child(), "starting a download should reveal the progress strip immediately");

        pump_until(
            || runtime.block_on(abs_storage::repo::download_tracks::get(&pool, &server.id, "item-1", "1")).unwrap().map(|r| r.status == abs_storage::models::DownloadStatus::Complete).unwrap_or(false),
            Duration::from_secs(10),
        );
        pump_until(|| !hooks.download_progress_revealer.reveals_child(), Duration::from_secs(5));
        assert!(!hooks.download_progress_revealer.reveals_child(), "the strip should retract once the download completes");

        window.destroy();
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Clicking a speed preset should call
    /// through to the real backend and update both the controller's snapshot and the button's
    /// label.
    pub(crate) fn run_speed_popover_changes_playback_speed(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();
        assert_eq!(hooks.speed_label.label(), "1.0×", "should start at the default speed");
        assert!(hooks.speed_button.popover().is_some(), "the speed button should open a popover");

        click_button_labeled(&hooks.speed_popover_box, "1.5×");
        assert_eq!(controller.snapshot().unwrap().speed, 1.5, "clicking a preset should change the real controller's speed");
        assert_eq!(hooks.speed_label.label(), "1.5×");

        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The full player's keyboard actions
    /// (ui-spec §6's full-player table): speed steps relative to the current speed, arrow-key
    /// skip against real (mock-served, seekable) audio, `c`/`t` popping their popovers, and
    /// Escape's collapse. The accelerators themselves are GTK-level (set app-wide in
    /// `application.rs`); what needs checking here is that the actions drive the same controller
    /// state their buttons do.
    pub(crate) fn run_keyboard_actions(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));

        let collapsed = std::rc::Rc::new(std::cell::Cell::new(false));
        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), {
            let collapsed = collapsed.clone();
            move || collapsed.set(true)
        }, || {});
        let actions = &screen.actions;

        assert_eq!(controller.snapshot().unwrap().speed, 1.0);
        actions.activate_action("speed-up", None);
        assert_eq!(controller.snapshot().unwrap().speed, 1.25, "speed-up should step to the next preset");
        actions.activate_action("speed-up", None);
        assert_eq!(controller.snapshot().unwrap().speed, 1.5);
        actions.activate_action("speed-down", None);
        assert_eq!(controller.snapshot().unwrap().speed, 1.25, "speed-down should step back");
        actions.activate_action("speed-reset", None);
        assert_eq!(controller.snapshot().unwrap().speed, 1.0, "reset should return to 1×");
        // At 1× already, reset is a no-op — no redundant seek-with-rate.
        actions.activate_action("speed-reset", None);
        assert_eq!(controller.snapshot().unwrap().speed, 1.0);

        // Arrow-key skip against the real pipeline: forward clamps to the 5s item's end, back
        // lands at (near) 0.
        actions.activate_action("skip-forward", None);
        pump_until(
            || (controller.snapshot().unwrap().position_seconds - 5.0).abs() < 1.0,
            Duration::from_secs(5),
        );
        actions.activate_action("skip-back", None);
        pump_until(|| controller.snapshot().unwrap().position_seconds < 1.0, Duration::from_secs(5));

        // `c`/`t` pop the same popovers their menu buttons open — which needs a mapped toplevel,
        // per `run_chapters_sheet_lists_and_seeks`'s note.
        let window = gtk4::Window::builder().child(&screen.root).build();
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));
        let hooks = screen.test_hooks();
        actions.activate_action("chapters", None);
        pump_until(|| hooks.chapters_popover.is_visible(), Duration::from_secs(2));
        actions.activate_action("sleep-timer", None);
        pump_until(|| hooks.sleep_timer_popover.is_visible(), Duration::from_secs(2));

        actions.activate_action("collapse", None);
        assert!(collapsed.get(), "Escape's action should call on_collapse");

        window.destroy();
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Exercises the real end-of-chapter
    /// deadline path (not a test-only backdoor): a very short first chapter, armed via the
    /// popover, should pause playback once the position crosses the chapter boundary.
    pub(crate) fn run_sleep_timer_end_of_chapter_pauses_at_the_boundary(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(crate::player::tests::mock_playable_item_with_chapters(
            &mock_server,
            "item-1",
            5,
            &[("Short Chapter", 0.0, 1.0), ("Rest", 1.0, 5.0)],
        ));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Chaptered Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| !controller.chapters().is_empty(), Duration::from_secs(10));

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();
        assert!(!hooks.sleep_timer_button.has_css_class("accent"), "no sleep timer armed yet");
        assert_eq!(hooks.sleep_timer_button.popover().as_ref(), Some(&hooks.sleep_timer_popover));

        click_button_labeled(&hooks.sleep_timer_popover_box, "End of chapter");
        assert!(controller.snapshot().unwrap().sleep_timer_active);
        assert!(hooks.sleep_timer_button.has_css_class("accent"), "the sleep timer button should show it's armed");

        pump_until(|| !controller.snapshot().unwrap().is_playing, Duration::from_secs(10));
        assert!(
            !controller.snapshot().unwrap().is_playing,
            "reaching the end of the current chapter should pause playback"
        );
        assert!(!controller.snapshot().unwrap().sleep_timer_active, "firing should also disarm the timer");

        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Clicking "Mark as finished" should
    /// pause playback and persist `is_finished = true` at the item's full duration, mirroring
    /// what actually reaching the end of a book does.
    pub(crate) fn run_mark_as_finished_button_updates_progress(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();

        hooks.mark_as_finished_button.emit_clicked();
        pump_until(|| !controller.snapshot().unwrap().is_playing, Duration::from_secs(5));
        assert!(!controller.snapshot().unwrap().is_playing, "marking finished should pause playback");

        pump_until(|| false, Duration::from_millis(300));
        let progress = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert!(progress.is_finished, "should be recorded as finished");
        assert_eq!(progress.current_time_seconds, 5.0, "should be recorded at the item's full duration");
        controller.stop();
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Clicking "Reset progress" should
    /// persist position 0 (not finished) and seek the actual, currently-loaded backend back to
    /// the start too, so the effect is visible without reopening the player.
    pub(crate) fn run_reset_progress_button_resets_position_and_seeks(runtime: &tokio::runtime::Runtime) {
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));

        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Test Book".to_string(), author: None },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| s.is_playing), Duration::from_secs(10));
        controller.skip(2.0);
        pump_until(|| controller.snapshot().unwrap().position_seconds > 1.0, Duration::from_secs(5));

        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool.clone()), || {}, || {});
        let hooks = screen.test_hooks();

        hooks.reset_progress_button.emit_clicked();
        pump_until(|| controller.snapshot().unwrap().position_seconds < 0.5, Duration::from_secs(5));
        assert!(controller.snapshot().unwrap().position_seconds < 0.5, "resetting should seek the loaded backend back to the start");

        pump_until(|| false, Duration::from_millis(300));
        let progress = runtime.block_on(abs_storage::repo::progress::get(&pool, &account.id, &server.id, "item-1")).unwrap().unwrap();
        assert!(!progress.is_finished);
        assert_eq!(progress.current_time_seconds, 0.0);
        controller.stop();
    }

    /// Finds and clicks the button with the given label anywhere inside a container, so tests can
    /// drive widgets the same way a user tapping them would. The text may sit on the button
    /// itself (the flat scope rows used to) or on a label inside it (scope rows are title +
    /// subtitle stacks now; "Next chapters" carries its title in its child box).
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

    /// Extracts the title label's text from a chapters-sheet row built by `build_chapter_row`.
    fn row_title(row: &gtk4::ListBoxRow) -> adw::glib::GString {
        let row_box = row.child().expect("row has a child box").downcast::<gtk4::Box>().unwrap();
        let title_label = row_box.first_child().expect("row box has a title label").downcast::<gtk4::Label>().unwrap();
        title_label.label()
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

    #[test]
    fn player_gesture_ignores_a_tap() {
        // Deliberately asymmetric with the mini bar's swipe-up gesture: a tap must not collapse
        // the full player.
        assert!(!player_gesture_should_collapse(0.0, 0.0));
    }

    #[test]
    fn player_gesture_recognizes_a_clean_swipe_down() {
        assert!(player_gesture_should_collapse(0.0, SWIPE_DOWN_MIN_DISTANCE_PX));
        assert!(player_gesture_should_collapse(2.0, SWIPE_DOWN_MIN_DISTANCE_PX + 1.0));
    }

    #[test]
    fn player_gesture_ignores_a_swipe_up() {
        assert!(!player_gesture_should_collapse(0.0, -SWIPE_DOWN_MIN_DISTANCE_PX));
    }

    #[test]
    fn player_gesture_ignores_a_horizontal_swipe() {
        assert!(!player_gesture_should_collapse(SWIPE_DOWN_MIN_DISTANCE_PX, 0.0));
    }

    #[test]
    fn player_gesture_ignores_a_mostly_horizontal_diagonal_drag() {
        // Crosses the vertical threshold, but the horizontal component dominates — not a clean
        // downward swipe, so this must not collapse the screen.
        assert!(!player_gesture_should_collapse(SWIPE_DOWN_MIN_DISTANCE_PX * 2.0, SWIPE_DOWN_MIN_DISTANCE_PX));
    }

    /// A loaded book with a cached (resized) cover, its Player screen as the page of a phone-sized
    /// window, and the original cover on the server — the setup every cover-viewer scenario
    /// below starts from.
    struct CoverScene {
        _mock_server: wiremock::MockServer,
        controller: crate::player::PlayerController,
        screen: PlayerScreen,
        window: adw::ApplicationWindow,
        collapsed: std::rc::Rc<std::cell::Cell<bool>>,
        server_id: String,
    }

    fn cover_scene(runtime: &tokio::runtime::Runtime, original_delay: Duration, original_requests: u64) -> CoverScene {
        use crate::widgets::cover_viewer::tests::{mount_original, seed_thumbnail};
        let mock_server = runtime.block_on(wiremock::MockServer::start());
        runtime.block_on(mock_playable_item(&mock_server, "item-1", 5));
        runtime.block_on(mount_original(&mock_server, "item-1", original_delay, original_requests));

        let pool = runtime.block_on(crate::test_support::pool());
        let (server, account) = runtime.block_on(account_and_server(&pool, &mock_server.uri()));
        runtime.block_on(insert_synced_item(&pool, &server.id, "item-1", "Test Item"));
        let controller = crate::player::PlayerController::new(pool.clone(), crate::test_support::test_paths(), test_backend(), |_| {});
        runtime.block_on(seed_thumbnail(&pool, &controller.paths(), &server.id, "item-1"));
        controller.start(
            abs_core::auth::Session::new(pool.clone(), &server, &account),
            PlayRequest { item_id: "item-1".to_string(), title: "Project Hail Mary".to_string(), author: Some("Andy Weir".to_string()) },
            1.0,
        );
        pump_until(|| controller.snapshot().is_some_and(|s| !s.is_loading), Duration::from_secs(10));

        let collapsed = std::rc::Rc::new(std::cell::Cell::new(false));
        let screen = build(pool.clone(), controller.clone(), test_download_manager(pool), {
            let collapsed = collapsed.clone();
            move || collapsed.set(true)
        }, || {});
        let window = adw::ApplicationWindow::builder().default_width(360).default_height(648).build();
        window.set_content(Some(&screen.root));
        window.present();
        pump_until(|| window.is_mapped(), Duration::from_secs(5));
        CoverScene { _mock_server: mock_server, controller, screen, window, collapsed, server_id: server.id }
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Tapping the Player's cover opens it
    /// full-screen at once with what was on screen, then the uncropped small copy with a
    /// download pill, then the full-size original; Escape closes the viewer, not the Player; a
    /// second open is served from the cache; ✕ closes it too.
    pub(crate) fn run_tapping_the_cover_opens_the_full_size_original(runtime: &tokio::runtime::Runtime) {
        use crate::widgets::cover_viewer::{self, tests::*, Pill};
        let scene = cover_scene(runtime, Duration::from_millis(1500), 1);
        let hooks = scene.screen.test_hooks();
        pump_until(|| hooks.cover.picture().is_visible(), Duration::from_secs(5));

        let tapped = std::time::Instant::now();
        hooks.cover.tap();
        let viewer = wait_until_shown(Duration::from_secs(2));
        assert!(tapped.elapsed() < Duration::from_millis(500), "the page opens at once: {:?}", tapped.elapsed());
        assert!(viewer.shown_texture().is_some(), "it opens with the cover that was on screen, never empty");

        wait_for_texture(&viewer, THUMBNAIL_SIZE, Duration::from_secs(5));
        assert!(
            matches!(viewer.pill(), Pill::Busy(ref text) if text.starts_with("Downloading full size")),
            "while the original downloads the pill says so: {:?}",
            viewer.pill()
        );

        wait_for_texture(&viewer, ORIGINAL_SIZE, Duration::from_secs(20));
        pump_until(|| viewer.pill() == Pill::Hidden, Duration::from_secs(2));
        assert_eq!(viewer.pill(), Pill::Hidden, "the pill goes once the original is shown");

        // Zoomed in, the page scrolls over the bigger image.
        viewer.zoom_for_test(3.0);
        pump_until(|| false, Duration::from_millis(300));
        let scroller = scroller(&viewer);
        assert!(scroller.hadjustment().upper() > scroller.hadjustment().page_size() * 2.0, "3× is wider than the page");
        assert_eq!(zoom_of(&viewer), 3.0);

        // Escape (the Player's `collapse` accelerator) closes the viewer only.
        scene.screen.actions.activate_action("collapse", None);
        pump_until(|| false, Duration::from_millis(200));
        assert!(viewer.is_closed());
        assert!(!scene.collapsed.get(), "the Player underneath stays open");
        assert_eq!(content_of(scene.window.upcast_ref()).as_ref(), Some(&scene.screen.root), "the Player page is back");
        assert!(viewer.shown_texture().is_none(), "the images are let go");

        // Again: from the cache this time (`expect(1)` on the original), closed with ✕.
        hooks.cover.tap();
        let viewer = wait_until_shown(Duration::from_secs(2));
        wait_for_texture(&viewer, ORIGINAL_SIZE, Duration::from_secs(20));
        close_button(&viewer).emit_clicked();
        pump_until(|| false, Duration::from_millis(200));
        assert!(cover_viewer::current().is_none());
        assert_eq!(content_of(scene.window.upcast_ref()).as_ref(), Some(&scene.screen.root));
        scene.controller.stop();
    }

    /// Closing before the original has arrived brings the Player back untouched, and the
    /// download still finishes into the cache for the next open.
    pub(crate) fn run_closing_the_cover_viewer_early_keeps_the_download(runtime: &tokio::runtime::Runtime) {
        use crate::widgets::cover_viewer::{self, tests::*};
        let scene = cover_scene(runtime, Duration::from_millis(1000), 1);
        let hooks = scene.screen.test_hooks();
        pump_until(|| hooks.cover.picture().is_visible(), Duration::from_secs(5));
        hooks.cover.tap();
        let viewer = wait_until_shown(Duration::from_secs(2));
        assert!(cover_viewer::close_current());
        pump_until(|| false, Duration::from_secs(3));
        assert!(viewer.is_closed() && viewer.shown_texture().is_none(), "nothing arriving later reopens or fills it");
        assert_eq!(content_of(scene.window.upcast_ref()).as_ref(), Some(&scene.screen.root));
        let paths = scene.controller.paths();
        assert!(
            runtime.block_on(abs_core::covers::cached_original_cover(&paths, &scene.server_id, "item-1")).is_some(),
            "the original was kept for the next open"
        );
        scene.controller.stop();
    }

    /// Low memory mode: the viewer opens with the small copy only, never fetching the original,
    /// says why, and zooms less far.
    pub(crate) fn run_the_cover_viewer_in_low_memory_mode_shows_the_small_copy(runtime: &tokio::runtime::Runtime) {
        use crate::widgets::cover_viewer::{self, tests::*, Pill};
        crate::widgets::cover_image::set_low_memory_mode(true);
        let scene = cover_scene(runtime, Duration::ZERO, 0);
        let hooks = scene.screen.test_hooks();
        pump_until(|| false, Duration::from_millis(300));
        hooks.cover.tap();
        let viewer = wait_until_shown(Duration::from_secs(2));
        wait_for_texture(&viewer, THUMBNAIL_SIZE, Duration::from_secs(5));
        assert_eq!(viewer.pill(), Pill::Note(cover_viewer::LOW_MEMORY_NOTE.to_string()));
        viewer.zoom_for_test(6.0);
        assert_eq!(zoom_of(&viewer), 3.0, "the small copy zooms to 3× at most");
        pump_until(|| false, Duration::from_secs(1));
        assert_eq!(texture_size(&viewer), Some((THUMBNAIL_SIZE.0 as i32, THUMBNAIL_SIZE.1 as i32)), "no original replaces it");
        viewer.close();
        pump_until(|| false, Duration::from_millis(200));
        assert!(viewer.shown_texture().is_none(), "the small copy is let go on close");
        crate::widgets::cover_image::set_low_memory_mode(false);
        scene.controller.stop();
        // `expect(0)` on the original is checked as the mock server drops.
    }

    /// Not a check: the cover viewer at a phone's window size — fitted, zoomed in on the
    /// original, and while the original is still downloading — in light and dark, into
    /// `$ABS_SCREENSHOT_DIR` (does nothing when that isn't set).
    pub(crate) fn run_cover_viewer_screenshots(runtime: &tokio::runtime::Runtime) {
        use crate::widgets::cover_viewer::{self, tests::*};
        let Some(dir) = std::env::var_os("ABS_SCREENSHOT_DIR").map(std::path::PathBuf::from) else { return };
        std::fs::create_dir_all(&dir).unwrap();
        let scene = cover_scene(runtime, Duration::from_secs(4), 1);
        let hooks = scene.screen.test_hooks();
        pump_until(|| hooks.cover.picture().is_visible(), Duration::from_secs(5));
        let style = adw::StyleManager::default();
        let shot = |name: &str| {
            pump_until(|| false, Duration::from_millis(600));
            crate::screens::main_window::tests::save_window_png(&scene.window, &dir.join(format!("{name}.png")));
        };
        style.set_color_scheme(adw::ColorScheme::ForceLight);
        shot("cover-0-player-light");
        hooks.cover.tap();
        let viewer = wait_until_shown(Duration::from_secs(2));
        wait_for_texture(&viewer, THUMBNAIL_SIZE, Duration::from_secs(5));
        shot("cover-1-downloading-light");
        wait_for_texture(&viewer, ORIGINAL_SIZE, Duration::from_secs(20));
        shot("cover-2-fitted-light");
        style.set_color_scheme(adw::ColorScheme::ForceDark);
        shot("cover-2-fitted-dark");
        viewer.zoom_for_test(3.0);
        shot("cover-3-zoomed-dark");
        assert!(cover_viewer::close_current());
        shot("cover-4-player-again-dark");
        style.set_color_scheme(adw::ColorScheme::Default);
        scene.controller.stop();
    }

    #[test]
    fn player_gesture_requires_the_full_swipe_distance() {
        assert!(!player_gesture_should_collapse(0.0, SWIPE_DOWN_MIN_DISTANCE_PX - 1.0));
    }
}
