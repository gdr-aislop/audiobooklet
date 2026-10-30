//! Visible progress for one item's in-flight download, on the screen that started it (Item
//! Detail, Player). Before this, starting a download gave only a "Download started" toast and
//! the download button's icon flipping to a static spinner glyph until the batch ended — no
//! chapter count, byte total, speed, or reachable Stop short of reopening the download popover,
//! and no route to the Downloads tab, which was also unreachable from either screen (both are
//! content-swapped over the shell, hiding the tab bar). See this feature's own plan for the fuller
//! reasoning; `widgets::download_scope_menu`'s "View" toast action and `screens::main_window`'s
//! Downloads-tab attention dot are this indicator's siblings, covering the moments after a toast
//! fades and after the user has navigated away.
//!
//! Shape mirrors `widgets::pull_to_refresh::PullIndicator` (a revealer + its handle), and reuses
//! `screens::downloads::downloading_subtitle`'s wording so the numbers read identically to the
//! Downloads tab's own live row rather than inventing a second wording for the same data. The
//! progress bar's fraction is coarser than that subtitle, though: chapters finished/total (from
//! `DownloadManager::batch_progress`), not a byte-weighted fraction across partially-downloaded
//! tracks — simpler, and still monotonic and honest; the subtitle underneath already carries the
//! byte/speed detail this would otherwise duplicate.

use adw::glib;
use adw::prelude::*;
use std::cell::RefCell;
use std::time::Instant;

use crate::download_speed::SpeedMeter;
use crate::downloads::{DownloadEvent, DownloadManager, ItemDownloadState};
use crate::i18n::{tr, tr_args};

pub(crate) struct DownloadProgressStrip {
    revealer: gtk4::Revealer,
    progress_bar: gtk4::ProgressBar,
    label: gtk4::Label,
    server_id: String,
    item_id: String,
    download_manager: DownloadManager,
    speed: RefCell<SpeedMeter>,
    /// Guards against stacking more than one deferred `update_label` per main-loop turn — several
    /// `TrackProgress`/`ItemStateChanged` events can land before the idle callback runs, and only
    /// the latest state matters.
    update_pending: std::cell::Cell<bool>,
}

impl DownloadProgressStrip {
    /// Builds the widget to insert into the screen's layout (Item Detail/Player both place it
    /// directly below their actions row) plus the handle that drives it, and registers the one
    /// `download_manager` listener that does so for this item's whole lifetime. If a batch is
    /// already in flight for this item at build time (the screen was reopened mid-download), the
    /// strip reveals immediately rather than waiting for the next event.
    pub(crate) fn build(download_manager: DownloadManager, server_id: String, item_id: String) -> (gtk4::Revealer, std::rc::Rc<Self>) {
        let progress_bar = gtk4::ProgressBar::builder().hexpand(true).build();
        let label = gtk4::Label::builder().css_classes(["caption", "dim-label"]).xalign(0.0).build();
        let stop_button = gtk4::Button::builder().label(&tr("Stop")).css_classes(["flat"]).valign(gtk4::Align::Center).build();

        let bar_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(8).build();
        bar_row.append(&progress_bar);
        bar_row.append(&stop_button);

        let content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(4).margin_top(10).build();
        content.append(&bar_row);
        content.append(&label);

        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideDown)
            .child(&content)
            .reveal_child(false)
            .build();

        let strip = std::rc::Rc::new(Self {
            revealer: revealer.clone(),
            progress_bar,
            label,
            server_id: server_id.clone(),
            item_id: item_id.clone(),
            download_manager: download_manager.clone(),
            speed: RefCell::new(SpeedMeter::default()),
            update_pending: std::cell::Cell::new(false),
        });

        stop_button.connect_clicked({
            let download_manager = download_manager.clone();
            let server_id = server_id.clone();
            let item_id = item_id.clone();
            move |_| download_manager.cancel_item(&server_id, &item_id)
        });

        download_manager.add_scoped_listener({
            let strip = strip.clone();
            move |event| {
                // Once the screen holding the strip is gone, the strip has no parent any more
                // (it is built attached to its screen, right away): stop listening.
                if strip.revealer.parent().is_none() {
                    return false;
                }
                Self::handle_event(&strip, event);
                true
            }
        });

        if download_manager.is_downloading(&server_id, &item_id) {
            strip.revealer.set_reveal_child(true);
            strip.schedule_update(&strip.clone());
        }

        (revealer, strip)
    }

    /// A plain function (not a `&self` method) taking the strip's own `Rc` explicitly: this runs
    /// from inside `download_manager`'s `publish()`, which several call sites reach while already
    /// holding `Inner`'s `RefCell` borrowed — `DownloadManager::batch_progress` (which
    /// `update_label` needs) borrows it again, so any recompute has to be deferred to a later
    /// main-loop turn via `schedule_update` rather than ever running synchronously in here.
    fn handle_event(strip: &std::rc::Rc<Self>, event: &DownloadEvent) {
        match event {
            DownloadEvent::ItemStateChanged { item_id, state } if *item_id == strip.item_id => match state {
                ItemDownloadState::Downloading => {
                    *strip.speed.borrow_mut() = SpeedMeter::default();
                    strip.revealer.set_reveal_child(true);
                    strip.schedule_update(strip);
                }
                ItemDownloadState::Idle | ItemDownloadState::Complete | ItemDownloadState::Stopped | ItemDownloadState::Failed(_) => {
                    strip.revealer.set_reveal_child(false);
                }
            },
            DownloadEvent::TrackProgress { item_id, ino, bytes_downloaded, .. } if *item_id == strip.item_id => {
                strip.speed.borrow_mut().record(ino, *bytes_downloaded, Instant::now());
                strip.schedule_update(strip);
            }
            _ => {}
        }
    }

    /// Defers `update_label` to the next main-loop iteration (see `handle_event`'s doc for why),
    /// coalescing any events that land before that turn runs into the one recompute.
    fn schedule_update(&self, strip: &std::rc::Rc<Self>) {
        if self.update_pending.replace(true) {
            return;
        }
        let strip = strip.clone();
        glib::idle_add_local_once(move || {
            strip.update_pending.set(false);
            strip.update_label();
        });
    }

    /// Recomputes the progress bar's fraction and the subtitle text from the manager's current
    /// batch state — called (via `schedule_update`) on every `TrackProgress` and whenever a batch
    /// starts.
    fn update_label(&self) {
        let batch = self.download_manager.batch_progress(&self.server_id, &self.item_id);
        if let Some((finished, total)) = batch {
            self.progress_bar.set_fraction(if total > 0 { finished as f64 / total as f64 } else { 0.0 });
        } else {
            self.progress_bar.pulse();
        }

        let (bytes, display_speed) = {
            let speed = self.speed.borrow();
            (speed.total(), speed.rate(Instant::now()))
        };

        let details = crate::screens::downloads::downloading_subtitle(batch, bytes, display_speed);
        // TRANSLATORS: {details} is a progress summary such as "3 of 12 chapters · 40 MB of 200 MB · 2.1 MB/s".
        self.label.set_label(&tr_args("Downloading · {details}", &[("details", &details)]));
    }
}
