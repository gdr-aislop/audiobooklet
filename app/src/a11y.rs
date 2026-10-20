//! Screen-reader support: accessible names, roles, states and focus for Orca.
//!
//! Everything here is invisible to a sighted user. A name or a role changes what Orca says, a
//! state change is spoken when the control has focus, and a focus move is followed by
//! `set_focus_visible(false)` so GTK draws no focus ring until the keyboard is used. Touch never
//! produces key events, so the key controllers here never run on the phone.
//!
//! The crate builds against GTK 4.8 (`app/Cargo.toml`), which decides the means:
//! - There are no live regions and no `gtk_accessible_announce` (4.14). Orca speaks a changed
//!   accessible name or description only for the widget that has focus, so feedback on an
//!   action goes on the control the user is resting on ([`name_if_changed`],
//!   [`describe_if_changed`]), or focus moves to a named widget ([`focus_now`]).
//! - There is no toggle-button role (4.10): a `GtkToggleButton` is a plain push button whose
//!   pressed state Orca never speaks. A group of exclusive toggles is exposed as radio buttons
//!   ([`radio`], "Author radio button selected, 2 of 4") and a standalone one as a switch
//!   ([`switch_like`], "Offline mode check box checked").
//! - A role can only be set before the widget's accessible context is realized — at build
//!   time, and before any `update_property` on the same widget, which creates the context.
//! - An icon-only button has no name at all (GTK 4.8 doesn't take it from the icon), so every one
//!   gets its tooltip text as its name ([`name`]).
//! - `adw::EntryRow`'s inner `GtkText` has no accessible role, so Orca never hears focus enter
//!   it; [`text_field`] gives it the row's title.

use adw::prelude::*;
use gtk4::accessible::{Property, Relation, State};
use gtk4::{glib, AccessibleRole, AccessibleTristate};

/// The accessible name — what Orca says first for the control.
pub(crate) fn name(widget: &impl IsA<gtk4::Accessible>, text: &str) {
    widget.update_property(&[Property::Label(text)]);
}

/// The accessible description — spoken after the name and role.
pub(crate) fn describe(widget: &impl IsA<gtk4::Accessible>, text: &str) {
    widget.update_property(&[Property::Description(text)]);
}

/// [`name`], but only when the text differs from the last one set here — for closures that run
/// on every player tick. Orca speaks the change if the control has focus.
pub(crate) fn name_if_changed(widget: &impl IsA<gtk4::Widget>, text: &str) {
    if remember(widget, "a11y-name", text) {
        name(widget.as_ref(), text);
    }
}

/// [`describe`], only when changed; see [`name_if_changed`].
pub(crate) fn describe_if_changed(widget: &impl IsA<gtk4::Widget>, text: &str) {
    if remember(widget, "a11y-description", text) {
        describe(widget.as_ref(), text);
    }
}

/// Stores `text` on the widget under `key`; whether it differed from what was stored.
fn remember(widget: &impl IsA<gtk4::Widget>, key: &str, text: &str) -> bool {
    let widget = widget.as_ref();
    // SAFETY: the key is only ever used with a `String` value, here.
    unsafe {
        if widget.data::<String>(key).is_some_and(|stored| stored.as_ref() == text) {
            return false;
        }
        widget.set_data(key, text.to_string());
    }
    true
}

/// A section heading (Orca: "Chapters heading"). The label keeps its text as its name.
pub(crate) fn heading(widget: &impl IsA<gtk4::Accessible>, level: i32) {
    widget.set_accessible_role(AccessibleRole::Heading);
    widget.update_property(&[Property::Level(level)]);
}

/// A screen's arrival point: a focusable heading named `text` that [`focus_landing`] moves
/// keyboard focus to when the screen appears, so Orca says where the user is ("Home heading").
pub(crate) fn landing(widget: &impl IsA<gtk4::Widget>, text: &str) {
    let widget = widget.as_ref();
    widget.set_focusable(true);
    heading(widget, 1);
    name(widget, text);
}

/// Focuses `widget` whenever it is mapped (a screen shown, a tab switched to, the shell
/// returning from the player), unless focus is already somewhere inside `screen` — Ctrl+F
/// focusing the search entry, for one — and never with a focus ring.
pub(crate) fn focus_landing(widget: &impl IsA<gtk4::Widget>, screen: &impl IsA<gtk4::Widget>) {
    let screen = screen.as_ref().downgrade();
    widget.as_ref().connect_map(move |widget| {
        let widget = widget.downgrade();
        let screen = screen.clone();
        // Content is swapped in an idle callback, and so is this: the old focus widget is gone by
        // now, and whatever the new screen focused on its own is in place.
        glib::idle_add_local_once(move || {
            let (Some(widget), Some(screen)) = (widget.upgrade(), screen.upgrade()) else { return };
            let Some(root) = widget.root() else { return };
            if root.focus().is_some_and(|focus| focus.is_ancestor(&screen) || focus == screen) {
                return;
            }
            focus_now(&widget);
        });
    });
}

/// Moves keyboard focus to `widget` without a focus ring: Orca speaks it, a sighted user sees
/// nothing.
pub(crate) fn focus_now(widget: &impl IsA<gtk4::Widget>) {
    let widget = widget.as_ref();
    if widget.grab_focus() {
        if let Some(window) = widget.root().and_downcast::<gtk4::Window>() {
            window.set_focus_visible(false);
        }
    }
}

/// One of a group of exclusive toggles (category chips, a mode switch): a radio button whose
/// checked state follows the toggle.
pub(crate) fn radio(toggle: &gtk4::ToggleButton) {
    toggle.set_accessible_role(AccessibleRole::Radio);
    set_checked(toggle, toggle.is_active());
    toggle.connect_toggled(|toggle| set_checked(toggle, toggle.is_active()));
}

/// One of a group of exclusive choices made with plain buttons (speed presets, sleep timer
/// options): a radio button whose checked state the caller keeps with [`set_option_checked`].
pub(crate) fn radio_option(button: &gtk4::Button) {
    button.set_accessible_role(AccessibleRole::Radio);
    set_checked(button, false);
}

pub(crate) fn set_option_checked(button: &gtk4::Button, checked: bool) {
    set_checked(button, checked);
}

/// A standalone on/off toggle (offline mode, grid/list): a switch whose checked state follows
/// the toggle, so Orca speaks "checked"/"not checked" as it flips.
pub(crate) fn switch_like(toggle: &gtk4::ToggleButton) {
    toggle.set_accessible_role(AccessibleRole::Switch);
    set_checked(toggle, toggle.is_active());
    toggle.connect_toggled(|toggle| set_checked(toggle, toggle.is_active()));
}

/// A list row holding a `GtkSwitch`: one Tab stop (the row) that reads as the switch, instead of
/// the row and then the switch inside it. Enter or space on the row flips the switch through the
/// row's activatable widget, as before.
pub(crate) fn switch_row(row: &adw::ActionRow, switch: &gtk4::Switch) {
    switch.set_focusable(false);
    row.set_accessible_role(AccessibleRole::Switch);
    set_checked(row, switch.is_active());
    switch.connect_active_notify({
        let row = row.clone();
        move |switch| set_checked(&row, switch.is_active())
    });
}

fn set_checked(widget: &impl IsA<gtk4::Accessible>, checked: bool) {
    widget.update_state(&[State::Checked(if checked { AccessibleTristate::True } else { AccessibleTristate::False })]);
}

/// Whether an expander (the "more" button under a description, an expander row's header) is
/// open.
pub(crate) fn expanded(widget: &impl IsA<gtk4::Accessible>, expanded: bool) {
    widget.update_state(&[State::Expanded(Some(expanded))]);
}

/// Names a widget after another one (a text view after its instructions).
pub(crate) fn labelled_by(widget: &impl IsA<gtk4::Accessible>, label: &impl IsA<gtk4::Accessible>) {
    widget.update_relation(&[Relation::LabelledBy(&[label.as_ref()])]);
}

/// Marks a field as holding a wrong value, with `message` as the error Orca can read.
pub(crate) fn invalid(widget: &impl IsA<gtk4::Accessible>, invalid: bool, message: Option<&impl IsA<gtk4::Accessible>>) {
    widget.update_state(&[State::Invalid(if invalid { gtk4::AccessibleInvalidState::True } else { gtk4::AccessibleInvalidState::False })]);
    match message {
        Some(message) if invalid => widget.update_relation(&[Relation::ErrorMessage(&[message.as_ref()])]),
        _ => widget.reset_relation(gtk4::AccessibleRelation::ErrorMessage),
    }
}

/// Gives an entry row's inner text widget (the one that takes focus) the row's title as its
/// name, so Orca says "Server URL text" when focus enters it.
pub(crate) fn text_field(row: &impl IsA<adw::EntryRow>) {
    let row = row.as_ref();
    let Some(text) = row.delegate() else { return };
    text.set_accessible_role(AccessibleRole::TextBox);
    name(&text, &row.title());
}

/// Makes `container` one Tab stop for keyboard users: Tab and Shift+Tab from inside it leave it
/// (the arrow keys move between its children as before), so a shelf of ten covers or a list of
/// seventy chapters doesn't take ten or seventy presses to pass.
pub(crate) fn focus_group(container: &impl IsA<gtk4::Widget>) {
    let controller = gtk4::EventControllerKey::new();
    controller.connect_key_pressed({
        let container = container.as_ref().downgrade();
        move |_, key, _, modifiers| {
            let backward = match key {
                gtk4::gdk::Key::Tab | gtk4::gdk::Key::KP_Tab => modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK),
                gtk4::gdk::Key::ISO_Left_Tab => true,
                _ => return glib::Propagation::Proceed,
            };
            let Some(container) = container.upgrade() else { return glib::Propagation::Proceed };
            let Some(root) = container.root() else { return glib::Propagation::Proceed };
            let direction = if backward { gtk4::DirectionType::TabBackward } else { gtk4::DirectionType::TabForward };
            // With the group unable to take focus, GTK's focus chain skips it as a whole, so the
            // next stop is the one after (or before) the group.
            let before = root.focus();
            container.set_can_focus(false);
            root.child_focus(direction);
            container.set_can_focus(true);
            if root.focus() != before {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    container.as_ref().add_controller(controller);
}

/// Keyboard activation for a widget that isn't a button but acts like one on tap (the mini
/// player's card, the player's chapter line): Enter or space runs `activate`.
pub(crate) fn activate_on_enter(widget: &impl IsA<gtk4::Widget>, activate: impl Fn() + 'static) {
    let controller = gtk4::EventControllerKey::new();
    controller.connect_key_pressed(move |_, key, _, _| match key {
        gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter | gtk4::gdk::Key::space => {
            activate();
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    widget.as_ref().add_controller(controller);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Not `#[test]`s — see `main.rs`'s `mod tests`: GTK is initialised once per scenario
    /// process, and this one runs every check below.
    pub(crate) fn run_helpers(_runtime: &tokio::runtime::Runtime) {
        a_name_is_read_back_and_only_set_again_when_it_changes();
        exclusive_toggles_are_radio_buttons_whose_checked_state_follows_them();
        a_standalone_toggle_is_a_switch_and_a_switch_row_is_one_stop();
        an_entry_row_s_text_is_named_after_the_row();
        a_landing_is_a_focusable_named_heading();
    }

    fn a_name_is_read_back_and_only_set_again_when_it_changes() {
        let button = gtk4::Button::new();
        assert_eq!(label_of(&button), None);
        name_if_changed(&button, "Play");
        assert_eq!(label_of(&button).as_deref(), Some("Play"));
        describe_if_changed(&button, "Chapter 2");
        assert_eq!(description_of(&button).as_deref(), Some("Chapter 2"));
        assert!(!remember(&button, "a11y-name", "Play"), "unchanged: nothing to emit");
        assert!(remember(&button, "a11y-name", "Pause"));
    }

    fn exclusive_toggles_are_radio_buttons_whose_checked_state_follows_them() {
        let first = gtk4::ToggleButton::builder().active(true).build();
        let second = gtk4::ToggleButton::builder().group(&first).build();
        radio(&first);
        radio(&second);
        assert_eq!(role_of(&first), AccessibleRole::Radio);
        assert!(is_checked(&first) && !is_checked(&second));
        second.set_active(true);
        assert!(!is_checked(&first) && is_checked(&second));
    }

    fn a_standalone_toggle_is_a_switch_and_a_switch_row_is_one_stop() {
        let toggle = gtk4::ToggleButton::new();
        switch_like(&toggle);
        assert_eq!(role_of(&toggle), AccessibleRole::Switch);
        assert!(!is_checked(&toggle));
        toggle.set_active(true);
        assert!(is_checked(&toggle));

        let switch = gtk4::Switch::new();
        let row = adw::ActionRow::builder().title("Remember password").activatable_widget(&switch).build();
        switch_row(&row, &switch);
        assert!(!switch.is_focusable(), "the row is the Tab stop");
        assert_eq!(role_of(&row), AccessibleRole::Switch);
        switch.set_active(true);
        assert!(is_checked(&row));
    }

    fn an_entry_row_s_text_is_named_after_the_row() {
        let row = adw::EntryRow::builder().title("Server URL").build();
        text_field(&row);
        let text = row.delegate().unwrap();
        assert_eq!(role_of(&text), AccessibleRole::TextBox);
        assert_eq!(label_of(&text).as_deref(), Some("Server URL"));
    }

    fn a_landing_is_a_focusable_named_heading() {
        let title = adw::WindowTitle::new("", "");
        landing(&title, "Home");
        assert!(title.is_focusable());
        assert_eq!(role_of(&title), AccessibleRole::Heading);
        assert_eq!(label_of(&title).as_deref(), Some("Home"));
    }

    /// What GTK holds as the widget's accessible name, or `None` when none is set.
    pub(crate) fn label_of(widget: &impl IsA<gtk4::Accessible>) -> Option<String> {
        string_property(widget, gtk4::ffi::GTK_ACCESSIBLE_PROPERTY_LABEL)
    }

    pub(crate) fn description_of(widget: &impl IsA<gtk4::Accessible>) -> Option<String> {
        string_property(widget, gtk4::ffi::GTK_ACCESSIBLE_PROPERTY_DESCRIPTION)
    }

    /// GTK has no getter for accessible properties; its test helper compares against a value and
    /// prints the real one when they differ, which is enough to read it back.
    fn string_property(widget: &impl IsA<gtk4::Accessible>, property: gtk4::ffi::GtkAccessibleProperty) -> Option<String> {
        use glib::translate::ToGlibPtr;
        let accessible: *mut gtk4::ffi::GtkAccessible = widget.as_ref().to_glib_none().0;
        // SAFETY: a plain FFI call with a valid widget and a NUL-terminated string argument.
        unsafe {
            if gtk4::ffi::gtk_test_accessible_has_property(accessible, property) == glib::ffi::GFALSE {
                return None;
            }
            let probe = c"\u{1}never the real value";
            let mismatch = gtk4::ffi::gtk_test_accessible_check_property(accessible, property, probe.as_ptr());
            if mismatch.is_null() {
                return Some(probe.to_str().unwrap().to_string());
            }
            let text = std::ffi::CStr::from_ptr(mismatch).to_string_lossy().into_owned();
            glib::ffi::g_free(mismatch as *mut _);
            Some(text)
        }
    }

    pub(crate) fn role_of(widget: &impl IsA<gtk4::Accessible>) -> AccessibleRole {
        widget.accessible_role()
    }

    pub(crate) fn is_checked(widget: &impl IsA<gtk4::Accessible>) -> bool {
        use glib::translate::ToGlibPtr;
        let accessible: *mut gtk4::ffi::GtkAccessible = widget.as_ref().to_glib_none().0;
        // SAFETY: a plain FFI call; the tristate is passed as the C int the function expects.
        unsafe {
            gtk4::ffi::gtk_test_accessible_check_state(accessible, gtk4::ffi::GTK_ACCESSIBLE_STATE_CHECKED, gtk4::ffi::GTK_ACCESSIBLE_TRISTATE_TRUE)
                .is_null()
        }
    }
}
