//! The shared "More options" `⋯` menu — "Mark as finished" and "Reset progress" — as a
//! `GtkMenuButton` + `GtkPopover`, same convention every other popover in this crate uses instead
//! of `GMenu`/`GAction` (see `widgets::download_scope_menu`'s own doc for the precedent).
//!
//! Extracted out of `screens::player` (see its git history) once `screens::item_detail` needed
//! the same two actions — one definition, not two copies that could quietly drift apart.
//!
//! "Reset progress" is styled `destructive-action` (it throws away the current listening
//! position) but, matching the ui-spec's own precedent for "Clear downloaded chapters", isn't
//! behind a confirmation dialog — `AdwAlertDialog` isn't available at this crate's libadwaita
//! v1.2 ceiling anyway, and being tucked inside a secondary menu is enough friction for something
//! this recoverable (nothing about the book itself is deleted). Note `destructive-action`
//! deliberately skips `flat`: the two combined render invisible (background-matching) text in
//! this popover's context — confirmed live, the original bug this shape already fixes once.

use adw::prelude::*;

use crate::i18n::tr;

/// The button + its popover. Callers embed `.widget` wherever their layout wants this action —
/// Player's header, Item Detail's header.
pub struct ItemOptionsMenu {
    pub widget: gtk4::MenuButton,
    #[cfg(test)]
    pub popover: gtk4::Popover,
    #[cfg(test)]
    pub mark_as_finished_button: gtk4::Button,
    #[cfg(test)]
    pub reset_progress_button: gtk4::Button,
}

/// `on_mark_as_finished` and `on_reset_progress` do the actual work; this widget only wires the click and closes the
/// popover. Deliberately no toast here any more (see this crate's git history for the "Marked as
/// finished"/"Progress reset" toasts this widget used to fire unconditionally, the instant the
/// button was clicked, before either callback's write had actually landed): both callbacks can
/// resolve asynchronously (item_detail's own not-currently-playing branch pushes to the server),
/// so only the caller — which awaits that — knows whether to report success or a real failure via
/// `error_reporting::report_background_error`.
pub fn build(
    on_mark_as_finished: impl Fn() + 'static,
    on_reset_progress: impl Fn() + 'static,
) -> ItemOptionsMenu {
    let mark_as_finished_button = gtk4::Button::builder().label(&tr("Mark as finished")).css_classes(["flat"]).halign(gtk4::Align::Start).build();
    let reset_progress_button = gtk4::Button::builder().label(&tr("Reset progress")).css_classes(["destructive-action"]).margin_top(6).build();

    let popover_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    popover_box.append(&mark_as_finished_button);
    popover_box.append(&reset_progress_button);

    let popover = gtk4::Popover::builder().child(&popover_box).build();
    let widget = gtk4::MenuButton::builder().icon_name("view-more-symbolic").tooltip_text(tr("More")).popover(&popover).build();
    crate::a11y::name(&widget, &tr("More"));

    mark_as_finished_button.connect_clicked({
        let popover = popover.clone();
        move |_| {
            on_mark_as_finished();
            popover.popdown();
        }
    });
    reset_progress_button.connect_clicked({
        let popover = popover.clone();
        move |_| {
            on_reset_progress();
            popover.popdown();
        }
    });

    ItemOptionsMenu {
        widget,
        #[cfg(test)]
        popover,
        #[cfg(test)]
        mark_as_finished_button,
        #[cfg(test)]
        reset_progress_button,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Clicking either button invokes the
    /// matching closure and closes the popover.
    pub(crate) fn run_buttons_invoke_their_callback_and_popdown(_runtime: &tokio::runtime::Runtime) {
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

        let marked_finished = Rc::new(Cell::new(false));
        let reset = Rc::new(Cell::new(false));
        let menu = build(
            {
                let marked_finished = marked_finished.clone();
                move || marked_finished.set(true)
            },
            {
                let reset = reset.clone();
                move || reset.set(true)
            },
        );
        content.append(&menu.widget);

        // A popover needs a real, mapped toplevel to `popup()`/`popdown()` against — same
        // requirement `screens::item_detail`'s own download-menu popover test already works
        // around the same way.
        let window = gtk4::Window::builder().child(&content).build();
        window.present();
        crate::test_support::pump_until(|| window.is_mapped(), std::time::Duration::from_secs(5));

        menu.popover.popup();
        menu.mark_as_finished_button.emit_clicked();
        assert!(marked_finished.get(), "clicking 'Mark as finished' should invoke the given callback");
        assert!(!menu.popover.is_visible(), "the popover should close after the action");

        menu.popover.popup();
        menu.reset_progress_button.emit_clicked();
        assert!(reset.get(), "clicking 'Reset progress' should invoke the given callback");
        assert!(!menu.popover.is_visible(), "the popover should close after the action");

        window.destroy();
    }
}
