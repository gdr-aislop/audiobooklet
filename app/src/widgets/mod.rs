//! Small reusable widgets shared across screens. Most of these are compatibility shims standing
//! in for libadwaita APIs newer than this crate's `v1_2` feature ceiling (PureOS Crimson's actual
//! shipped version — see the architecture plan), each documenting exactly which real widget/
//! version it's covering for so it can be deleted the day the minimum target version moves past
//! it; `cover_image` is the exception — a plain graceful-degradation wrapper, not a version shim,
//! and `pull_to_refresh` is another: GTK4/libadwaita simply don't ship the gesture at all.

pub mod banner;
pub mod connection_editors;
pub mod cover_image;
pub mod cover_viewer;
pub mod download_progress;
pub mod download_scope_menu;
pub mod item_card;
pub mod item_options_menu;
pub mod pull_to_refresh;
pub mod sideways_scroll;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::glib;
use adw::prelude::*;

use crate::i18n::tr;

/// Swaps `window`'s content one main-loop idle tick later than requested, instead of immediately.
/// GTK's click/gesture/row-activation handling expects the widget it's currently processing a
/// press/release for to still exist in its original ancestor chain when that handling finishes;
/// calling `window.set_content(...)` synchronously *inside* that same widget's own handler
/// unparents (and can finalize) it mid-bookkeeping, which is exactly what produces GTK's "Broken
/// accounting of active state for widget" warning. Deferring by one idle callback lets the
/// originating event finish unwinding first. Every window-content swap in this app should go
/// through here rather than calling `set_content` directly, so this can't regress silently.
pub(crate) fn swap_content(window: &adw::ApplicationWindow, content: &impl glib::object::IsA<gtk4::Widget>) {
    let window = window.clone();
    let content = content.clone().upcast::<gtk4::Widget>();
    glib::idle_add_local_once(move || window.set_content(Some(&content)));
}

/// Walks `root`'s descendant tree depth-first looking for the first widget of type `T` — how
/// tests and this crate's few styled-parent cases reach into widgets GTK won't hand out handles
/// to (a `GtkMessageDialog`'s internal entry, an `AdwToast`'s title label).
pub(crate) fn find_descendant<T: glib::object::IsA<gtk4::Widget>>(root: &gtk4::Widget) -> Option<T> {
    if let Some(found) = root.downcast_ref::<T>() {
        return Some(found.clone());
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Some(found) = find_descendant::<T>(&widget) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}

/// An `AdwComboRow` over a plain list of option labels — the string expression is what makes the
/// row display the selected option's text in its trailing slot. Shared by `screens::settings` and
/// `screens::library`'s view-options popover, so both build combo rows the same way.
pub(crate) fn combo_row(title: &str, subtitle: &str, options: &[String]) -> adw::ComboRow {
    let model = gtk4::StringList::new(&[]);
    for option in options {
        model.append(option);
    }
    let row = adw::ComboRow::builder().title(title).subtitle(subtitle).model(&model).build();
    row.set_expression(Some(gtk4::StringObject::this_expression("string")));
    // A screen reader hears the row's title and subtitle but not its trailing value, so the
    // value leads the description: "Default speed combo box. 1.0×. Speed every book starts at."
    // libadwaita describes the row by its subtitle label, which GTK 4.14 prefers to a
    // description set here (4.8 does the opposite); dropping that relation makes this the
    // description on both.
    row.reset_relation(gtk4::AccessibleRelation::DescribedBy);
    let describe = {
        let subtitle = subtitle.to_string();
        move |row: &adw::ComboRow| {
            let selected = row.selected_item().and_downcast::<gtk4::StringObject>().map(|option| option.string().to_string()).unwrap_or_default();
            let spoken = if subtitle.is_empty() { selected } else { format!("{selected}. {subtitle}") };
            crate::a11y::describe_if_changed(row, &spoken);
        }
    };
    describe(&row);
    row.connect_selected_item_notify(describe);
    row
}

/// Sets `GtkEditable:input-purpose` on an `AdwEntryRow` — which libadwaita doesn't expose as a
/// row property: the purpose lives on the row's internal `GtkText` editable delegate. The
/// on-screen keyboard picks its layout from this (squeekboard shows its URL layout — `/`, `:`,
/// no autocapitalization — only when the focused entry's purpose is `URL`), so fields that hold
/// addresses must set it or they get the plain text keyboard.
pub(crate) fn entry_row_input_purpose(row: &adw::EntryRow, purpose: gtk4::InputPurpose) {
    let text = row
        .delegate()
        .and_then(|delegate| delegate.downcast::<gtk4::Text>().ok())
        .expect("AdwEntryRow's editable delegate should be a GtkText");
    text.set_input_purpose(purpose);
}

/// What a *manual* sync trigger — a screen's ⋯ menu "Sync now" item, or the pull-to-refresh
/// gesture — adds to that screen's sync cycle: a pull indicator revealed for the sync's duration
/// (otherwise the only feedback a pull gesture gets is invisible), a toast reporting the outcome
/// (the banner/empty state stay the detailed failure surface), and the in-flight flag both of a
/// screen's triggers share so syncs can't stack (an overshot can fire repeatedly during one
/// rubber-band, and the menu is one tap away from the gesture). The automatic cycle and the empty
/// state's "Try again" pass none — they already own their feedback, and re-running them is
/// deliberately unguarded.
///
/// Cheap to clone: every field is reference-counted, and the clones share the one flag.
#[derive(Clone)]
pub(crate) struct ManualSync {
    toast_overlay: adw::ToastOverlay,
    indicator: crate::widgets::pull_to_refresh::PullIndicator,
    in_flight: Rc<Cell<bool>>,
}

impl ManualSync {
    pub(crate) fn new(
        toast_overlay: &adw::ToastOverlay,
        indicator: &crate::widgets::pull_to_refresh::PullIndicator,
    ) -> Self {
        Self {
            toast_overlay: toast_overlay.clone(),
            indicator: indicator.clone(),
            in_flight: Rc::new(Cell::new(false)),
        }
    }

    /// Claims the manual-sync slot, or reports it taken. Called by the sync cycle when it's
    /// spawned, so both trigger sites share the guard without knowing about each other. Reveals
    /// the pull indicator the same frame the trigger is recognized — the only visual feedback a
    /// pull gesture gets, since `edge-overshot` carries no drag-distance payload to animate.
    pub(crate) fn claim(&self) -> bool {
        if self.in_flight.get() {
            return false;
        }
        self.in_flight.set(true);
        self.indicator.show();
        true
    }

    /// Releases the slot, retracts the pull indicator, and reports the outcome as a transient
    /// toast. Called at the sync cycle's resolve step, so the toast lands only after the screen
    /// has rendered the result.
    pub(crate) fn finish(&self, ok: bool) {
        self.in_flight.set(false);
        self.indicator.hide();
        let message = if ok { tr("Sync complete") } else { tr("Sync failed") };
        self.toast_overlay.add_toast(adw::Toast::new(&message));
    }

    /// Releases the slot without syncing: offline mode is on, so nothing may reach the server.
    pub(crate) fn finish_offline(&self) {
        self.in_flight.set(false);
        self.indicator.hide();
        self.toast_overlay.add_toast(adw::Toast::new(&tr("Offline mode is on — turn it off to sync")));
    }
}

/// A cancel-and-reschedule debounce over a one-shot GLib timeout — call [`Self::schedule`] on
/// every raw event (a keystroke, a scroll position change); only the last call within `delay`
/// of the previous one actually runs its `action`.
///
/// Correctness invariant this exists to enforce: `pending` holds a `SourceId` only while that
/// source is still genuinely alive. A GLib `_once` source destroys itself right after its
/// callback returns — so the callback must clear `pending` *before* running `action`, not after.
/// Getting this backwards (as an earlier version of this debounce did, once per call site,
/// hand-rolled) means a later `schedule` calls `take()`, finds the dead id still sitting there,
/// and calls `.remove()` on it — which GLib rejects (`Source ID … was not found`) and glib-rs
/// turns into a panic. That panic fires from inside a GTK signal handler, which can't unwind
/// across the C boundary, so the whole process aborts. This happened for real: scrolling twice
/// (or typing twice) with more than the debounce delay between each action reliably crashed the
/// app. One correct implementation, used by every debounced call site, instead of every call
/// site getting this ordering right (or wrong) on its own.
#[derive(Clone, Default)]
pub(crate) struct Debouncer {
    pending: Rc<RefCell<Option<glib::SourceId>>>,
}

impl Debouncer {
    pub(crate) fn schedule(&self, delay: Duration, action: impl FnOnce() + 'static) {
        if let Some(id) = self.pending.borrow_mut().take() {
            id.remove();
        }
        let pending = self.pending.clone();
        let id = glib::timeout_add_local_once(delay, move || {
            // The source is about to be destroyed (this is a `_once` timeout) — forget its id
            // first, so nothing can later try to remove an id GLib is about to invalidate.
            pending.borrow_mut().take();
            action();
        });
        *self.pending.borrow_mut() = Some(id);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::pump_until;

    /// The exact sequence that used to abort the process: schedule, let it fire, schedule
    /// *again* — the second `schedule` must not try to remove the first (already self-destroyed)
    /// source. Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast
    /// GTK-touching scenario in this binary has to run from one single entry point.
    pub(crate) fn run_fires_again_after_a_previous_debounce_already_fired(_runtime: &tokio::runtime::Runtime) {
        let debouncer = Debouncer::default();
        let first_ran = Rc::new(Cell::new(false));
        debouncer.schedule(Duration::from_millis(20), {
            let first_ran = first_ran.clone();
            move || first_ran.set(true)
        });
        pump_until(|| first_ran.get(), Duration::from_secs(5));

        // This is the call that used to panic (and abort the whole process, since it happens
        // inside a GLib source callback) — the first schedule's `_once` source already
        // destroyed itself once it fired above.
        let second_ran = Rc::new(Cell::new(false));
        debouncer.schedule(Duration::from_millis(20), {
            let second_ran = second_ran.clone();
            move || second_ran.set(true)
        });
        pump_until(|| second_ran.get(), Duration::from_secs(5));
        assert!(second_ran.get(), "a second, later schedule must still fire normally");
    }

    /// The actual debounce behavior: scheduling again before the delay elapses cancels the
    /// first action rather than running both.
    pub(crate) fn run_only_the_last_schedule_within_the_delay_runs(_runtime: &tokio::runtime::Runtime) {
        let debouncer = Debouncer::default();
        let runs: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

        debouncer.schedule(Duration::from_millis(200), {
            let runs = runs.clone();
            move || runs.borrow_mut().push("first")
        });
        debouncer.schedule(Duration::from_millis(200), {
            let runs = runs.clone();
            move || runs.borrow_mut().push("second")
        });

        pump_until(|| !runs.borrow().is_empty(), Duration::from_secs(5));
        assert_eq!(*runs.borrow(), vec!["second"], "only the later schedule should have run");
    }
}

/// `text` with invisible line-break opportunities after `.`, `_` and `/` inside long unspaced
/// runs, for labels showing a title or author that came from a file or folder name
/// (`Andrzej.Pilipiuk.Wojslawicka.masakra`). Line breaking follows Unicode's rules, which allow a
/// break after a hyphen but not after a dot or underscore between letters, so such a name is one
/// unbreakable word, and `WrapMode::WordChar` alone cuts it wherever it runs out of room. Pango has
/// no attribute that adds break opportunities, hence a zero-width space (U+200B) in the displayed
/// text. Only a run of 12 or more characters without whitespace is touched, so ordinary titles
/// and initials ("J.R.R. Tolkien") come back unchanged. For display only: names for screen readers
/// and anything stored or searched use the original text, and selecting and copying the label
/// copies the invisible spaces too.
pub fn breakable(text: &str) -> std::borrow::Cow<'_, str> {
    const MIN_RUN: usize = 12;
    const ZERO_WIDTH_SPACE: char = '\u{200b}';
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for piece in text.split_inclusive(char::is_whitespace) {
        let run = piece.trim_end_matches(char::is_whitespace);
        if run.chars().count() < MIN_RUN {
            out.push_str(piece);
            continue;
        }
        let mut chars = run.chars().peekable();
        while let Some(c) = chars.next() {
            out.push(c);
            if matches!(c, '.' | '_' | '/') && chars.peek().is_some_and(|next| *next != ZERO_WIDTH_SPACE) {
                out.push(ZERO_WIDTH_SPACE);
                changed = true;
            }
        }
        out.push_str(&piece[run.len()..]);
    }
    if changed {
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod breakable_tests {
    use super::breakable;

    const Z: &str = "\u{200b}";

    #[test]
    fn ordinary_text_comes_back_unchanged() {
        for text in ["", "Piranesi", "J.R.R. Tolkien", "Mr. Smith", "Project Hail Mary", "a.b.c", "Krzysztof Kolumb. Odkrywca z wyspy Chios"] {
            assert!(matches!(breakable(text), std::borrow::Cow::Borrowed(_)), "{text:?}");
            assert_eq!(breakable(text).replace(Z, "").as_str(), text);
        }
        assert_eq!(breakable("J.R.R. Tolkien"), "J.R.R. Tolkien");
        assert_eq!(breakable("Mr. Smith"), "Mr. Smith");
    }

    #[test]
    fn a_long_dotted_name_gets_a_break_after_each_separator() {
        let title = "Andrzej.Pilipiuk.Wojslawicka.masakra.kosa.lancuch.gyky";
        let shown = breakable(title);
        assert_eq!(shown.matches(Z).count(), 6);
        assert_eq!(shown.replace(Z, ""), title);
        assert!(shown.starts_with(&format!("Andrzej.{Z}Pilipiuk.{Z}")));
        assert_eq!(breakable("some_long_file_name_here"), format!("some_{Z}long_{Z}file_{Z}name_{Z}here"));
        assert_eq!(breakable("path/to/some/where/else"), format!("path/{Z}to/{Z}some/{Z}where/{Z}else"));
    }

    #[test]
    fn edges_and_odd_input_are_safe() {
        // A trailing separator gets no break after it; runs of separators each get one except at the end.
        assert_eq!(breakable("abcdefghijkl."), "abcdefghijkl.");
        assert_eq!(breakable("abcdefgh...ijkl"), format!("abcdefgh.{Z}.{Z}.{Z}ijkl"));
        // Only the long run in a mixed string is touched, and whitespace is kept as it was.
        assert_eq!(breakable("Intro  to.a_very_long_file.name  end"), format!("Intro  to.a_very_long_file.name  end").replace("to.a_very_long_file.name", &format!("to.{Z}a_{Z}very_{Z}long_{Z}file.{Z}name")));
        // Multi-byte characters, and text that already has the invisible space.
        assert_eq!(breakable("Żółć.Gęślą.Jaźń.Ąę"), format!("Żółć.{Z}Gęślą.{Z}Jaźń.{Z}Ąę"));
        assert_eq!(breakable(&format!("abcdefgh.{Z}ijklmnop")), format!("abcdefgh.{Z}ijklmnop"));
        assert_eq!(breakable("   "), "   ");
    }
}
