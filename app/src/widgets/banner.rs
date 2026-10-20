//! A minimal stand-in for `AdwBanner`, which turns out to require libadwaita **1.3** — one
//! version past this crate's `v1_2` feature ceiling. Confirmed by reading libadwaita-rs's own
//! generated feature gates (`mod banner;` in its `auto/mod.rs` is behind `#[cfg(feature =
//! "v1_3")]`), not assumed; the ceiling caught this at compile time exactly as intended, rather
//! than as a runtime surprise on Crimson.
//!
//! Only the two properties this app actually needs are covered — a title and a revealed/hidden
//! state — as a plain `GtkRevealer` wrapping a small icon+label row. Delete this and switch back
//! to `adw::Banner` (same method names, deliberately) the day the app's minimum libadwaita moves
//! to 1.3+.

#[cfg(test)]
use adw::glib;
use adw::prelude::*;

use crate::i18n::tr;

#[derive(Clone)]
pub struct ErrorBanner {
    revealer: gtk4::Revealer,
    /// The card inside the revealer: the alert a screen reader is moved to.
    content: gtk4::Box,
    label: gtk4::Label,
    description: gtk4::Label,
    bottom_row: gtk4::Box,
    details_toggle: gtk4::ToggleButton,
    details_label: gtk4::Label,
    action_button: gtk4::Button,
    /// Set by [`Self::show_sync_failure`]: whether the action button currently means "Log in
    /// again" rather than "Retry", for the screen's single click handler to dispatch on.
    offers_login: std::rc::Rc<std::cell::Cell<bool>>,
}

/// The banner's card look, loaded once per process (the same `Once`-guarded `CssProvider` idiom
/// as `player::ensure_mini_bar_css`). An amber tint rather than red: a server that can't be
/// reached isn't something the user did wrong, and the text stays in the normal foreground
/// colour, which reads well in light and dark alike. Named colours only, so high-contrast and
/// dark follow by themselves. Also defines `button.app-link`: a borderless, accent-coloured text
/// button for "Details", here and on Home's full-page error, which never looks pressed.
pub(crate) fn ensure_banner_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(
            "box.app-banner { background-color: alpha(@warning_color, 0.15); border-radius: 12px; padding: 12px; } \
             box.app-banner image.app-banner-icon { color: @warning_color; } \
             box.app-banner label.app-banner-title { font-weight: 700; } \
             box.app-banner label.app-banner-details { font-family: monospace; } \
             button.app-link, button.app-link:hover, button.app-link:active, button.app-link:checked { \
                 background: none; box-shadow: none; padding: 4px 0; min-height: 0; color: @accent_color; }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("a display for the app's css"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

impl ErrorBanner {
    /// Laid out as a card a little in from the screen's edges:
    ///
    /// ```text
    ///  ⚠  Headline (bold)
    ///     Description (dimmed, optional)
    ///     Details ›                 [ Action ]
    ///     (the details text, when opened)
    /// ```
    pub fn new() -> Self {
        ensure_banner_css();
        let icon = gtk4::Image::builder().icon_name("dialog-warning-symbolic").valign(gtk4::Align::Start).css_classes(["app-banner-icon"]).build();
        // `max_width_chars(1)` caps each label's *natural* width request regardless of `wrap` —
        // an unclamped label asks Pango for enough room to lay out the whole message on one line
        // before wrapping, and this banner sits in a plain `Box` with no horizontal scroller
        // anywhere in its ancestry (`home.rs`/`library.rs` append it straight to their root
        // `Box`), so a long server/network error message could otherwise force the whole window
        // wider than the screen — the exact failure mode `widgets::item_card`'s `title_label` doc
        // comment already documents once, for a different label.
        let label = gtk4::Label::builder().wrap(true).max_width_chars(1).xalign(0.0).hexpand(true).css_classes(["app-banner-title"]).build();
        let description =
            gtk4::Label::builder().wrap(true).max_width_chars(1).xalign(0.0).hexpand(true).css_classes(["dim-label"]).visible(false).build();

        // The optional action (Retry, Log in again). Hidden unless `set_action_label` says
        // otherwise; visibility is relative to the revealer's, so `set_revealed` keeps owning
        // whether the banner shows at all.
        let action_button = gtk4::Button::builder().halign(gtk4::Align::End).valign(gtk4::Align::Center).hexpand(true).visible(false).build();

        // Collapsed by default: raw HTTP/TLS library text isn't meant for a general audience, but
        // a self-hosted user debugging an unusual TLS/proxy setup benefits from being able to see
        // (and copy, via `selectable`) the exact underlying error. Hidden entirely — not just
        // collapsed — when there's nothing to show (see `set_details`).
        let details_toggle =
            gtk4::ToggleButton::builder().label(tr("Details")).css_classes(["app-link"]).halign(gtk4::Align::Start).valign(gtk4::Align::Center).visible(false).build();
        let details_label = gtk4::Label::builder()
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .max_width_chars(1)
            .xalign(0.0)
            .selectable(true)
            .css_classes(["dim-label", "caption", "app-banner-details"])
            .build();
        let details_revealer = gtk4::Revealer::builder().child(&details_label).reveal_child(false).build();
        details_toggle.connect_toggled({
            let details_revealer = details_revealer.clone();
            move |toggle| {
                details_revealer.set_reveal_child(toggle.is_active());
                toggle.set_label(&if toggle.is_active() { tr("Hide details") } else { tr("Details") });
                crate::a11y::expanded(toggle, toggle.is_active());
            }
        });

        let bottom_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(8).visible(false).build();
        bottom_row.append(&details_toggle);
        bottom_row.append(&action_button);

        let text = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(4).hexpand(true).build();
        text.append(&label);
        text.append(&description);
        text.append(&bottom_row);
        text.append(&details_revealer);

        // For a screen reader the whole card is one alert, named by its headline and
        // description, that `focus` can move to (there are no live regions in GTK 4.8).
        let content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(10)
            .margin_top(8)
            .margin_bottom(4)
            .margin_start(12)
            .margin_end(12)
            .css_classes(["app-banner"])
            .accessible_role(gtk4::AccessibleRole::Alert)
            .focusable(true)
            .build();
        content.append(&icon);
        content.append(&text);
        crate::a11y::expanded(&details_toggle, false);

        let revealer = gtk4::Revealer::builder()
            .transition_type(gtk4::RevealerTransitionType::SlideDown)
            .child(&content)
            .reveal_child(false)
            .build();

        Self { revealer, content, label, description, bottom_row, details_toggle, details_label, action_button, offers_login: Default::default() }
    }

    /// The bottom row only takes space when it has something in it. `get_visible` (the
    /// children's own flag), not `is_visible`, which is also false while this row is hidden.
    fn update_bottom_row(&self) {
        self.bottom_row.set_visible(self.details_toggle.get_visible() || self.action_button.get_visible());
    }

    pub fn widget(&self) -> &gtk4::Revealer {
        &self.revealer
    }

    /// The short, bold headline — what happened, in a few words.
    pub fn set_title(&self, text: &str) {
        self.label.set_label(text);
        self.refresh_name();
    }

    /// The alert's accessible name: headline and description in one, what Orca reads when
    /// [`focus`](Self::focus) lands here.
    fn refresh_name(&self) {
        let mut spoken = self.label.label().to_string();
        if self.description.is_visible() {
            spoken.push_str(". ");
            spoken.push_str(&self.description.label());
        }
        crate::a11y::name(&self.content, &spoken);
    }

    /// Moves keyboard focus to the banner so a screen reader speaks it — once the revealer has
    /// shown it, hence the idle. A sighted user sees no focus ring.
    pub fn focus(&self) {
        let content = self.content.clone();
        adw::glib::idle_add_local_once(move || crate::a11y::focus_now(&content));
    }

    /// The optional dimmed line under the headline — what still works, or what to do.
    pub fn set_description(&self, text: Option<&str>) {
        self.description.set_label(text.unwrap_or(""));
        self.description.set_visible(text.is_some_and(|t| !t.is_empty()));
        self.refresh_name();
    }

    #[cfg(test)]
    pub fn has_focus(&self) -> bool {
        self.content.has_focus()
    }

    #[cfg(test)]
    pub fn accessible_name(&self) -> Option<String> {
        crate::a11y::tests::label_of(&self.content)
    }

    #[cfg(test)]
    pub fn title(&self) -> glib::GString {
        self.label.label()
    }

    /// `Some(text)` shows the collapsed "Details" toggle with `text` behind it; `None` hides
    /// it entirely (also collapsing it, so it doesn't reopen already-expanded next time
    /// details are shown for an unrelated error).
    pub fn set_details(&self, details: Option<&str>) {
        match details {
            Some(text) => {
                self.details_label.set_label(text);
                self.details_toggle.set_visible(true);
            }
            None => {
                self.details_toggle.set_active(false);
                self.details_toggle.set_visible(false);
            }
        }
        self.update_bottom_row();
    }

    /// The optional trailing action button (e.g. "Log in again" on an authorization failure).
    /// `Some(label)` shows the button; wire its handler via [`Self::action_button`] once per
    /// banner — the label is pure state, the handler survives relabeling.
    pub fn set_action_label(&self, label: Option<&str>) {
        match label {
            Some(text) => {
                self.action_button.set_label(text);
                self.action_button.set_visible(true);
            }
            None => self.action_button.set_visible(false),
        }
        self.update_bottom_row();
    }

    pub fn action_button(&self) -> &gtk4::Button {
        &self.action_button
    }

    #[cfg(test)]
    pub fn action_label(&self) -> glib::GString {
        self.action_button.label().unwrap_or_default()
    }

    #[cfg(test)]
    pub fn action_visible(&self) -> bool {
        self.action_button.is_visible()
    }

    #[cfg(test)]
    pub fn details_visible(&self) -> bool {
        self.details_toggle.is_visible()
    }

    #[cfg(test)]
    pub fn description(&self) -> glib::GString {
        self.description.label()
    }

    #[cfg(test)]
    pub fn open_details(&self) {
        self.details_toggle.set_active(true);
    }

    #[cfg(test)]
    pub fn details_text(&self) -> glib::GString {
        self.details_label.label()
    }

    /// Home's and Library's banner for a sync that failed while there's saved data to keep
    /// showing — one wording for both screens:
    ///
    /// - the server never answered: "Can't reach your server", with Retry;
    /// - it answered, but not usefully: "Couldn't update your library", with Retry;
    /// - it signed this session out: "Signed out by the server", with Log in again.
    ///
    /// The screen's action handler asks [`Self::offers_login`] which of the two to run.
    pub fn show_sync_failure(&self, err: &abs_core::CoreError) {
        let (title, description, login) = match err {
            abs_core::CoreError::Auth => {
                (tr("Signed out by the server"), tr("Showing what's saved on this device. Log in again to update it."), true)
            }
            abs_core::CoreError::Unreachable(_) => (tr("Can't reach your server"), tr("Showing what's saved on this device."), false),
            _ => (tr("Couldn't update your library"), tr("Showing what's saved on this device."), false),
        };
        self.set_title(&title);
        self.set_description(Some(&description));
        self.offers_login.set(login);
        self.set_action_label(Some(&if login { tr("Log in again") } else { tr("Retry") }));
        self.set_details(Some(&err.to_string()));
        self.set_revealed(true);
    }

    /// Whether the action button currently means "Log in again" (see [`Self::show_sync_failure`]).
    pub fn offers_login(&self) -> bool {
        self.offers_login.get()
    }

    pub fn set_revealed(&self, revealed: bool) {
        self.revealer.set_reveal_child(revealed);
    }
}

impl Default for ErrorBanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Every part of the banner wraps: with
    /// a long headline, description and an unbroken URL in the opened details, it still fits a
    /// 360px phone (nothing on these screens scrolls sideways, so a wider banner would widen the
    /// whole window). The bottom row only shows when it has something in it.
    pub(crate) fn run_the_banner_fits_a_phone_and_hides_an_empty_bottom_row(_rt: &tokio::runtime::Runtime) {
        let banner = ErrorBanner::new();
        banner.set_title("Can't reach your server, which is a fairly long headline for a banner");
        banner.set_description(Some("Showing what's saved on this device, and a description long enough to wrap twice."));
        assert!(!banner.bottom_row.get_visible(), "no details and no action: no bottom row");

        banner.set_action_label(Some("Retry"));
        banner.set_details(Some(&format!("couldn't connect to the server: http://{}/api/libraries", "a".repeat(200))));
        assert!(banner.bottom_row.get_visible() && banner.action_visible() && banner.details_visible());
        banner.open_details();
        banner.set_revealed(true);

        let (min_width, ..) = banner.widget().measure(gtk4::Orientation::Horizontal, -1);
        assert!(min_width <= 360, "the banner needs {min_width}px, wider than a phone");

        banner.set_details(None);
        banner.set_action_label(None);
        assert!(!banner.bottom_row.get_visible(), "an emptied bottom row hides again");
    }
}
