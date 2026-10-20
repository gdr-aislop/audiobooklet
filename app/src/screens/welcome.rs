//! The Welcome/Server login screen — shown whenever there's no usable session: first launch
//! (`abs_storage::repo::accounts::get_active` returns `None`), or as the re-login flow when a
//! dead session's "Log in again" action hands control back from the shell. See
//! `docs/design/ui-spec.md`'s "Welcome / Server login" section and the published mockup for the
//! visual design this implements.

use std::rc::Rc;

use abs_core::accounts::{AddedAccount, ClientCertificate, ConnectionSettings, ReloginSeed, ReplacementKind, SignIn};
use abs_core::error::CoreError;
use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use crate::i18n::{tr, tr_args};
use crate::widgets::connection_editors::{
    cert_subtitle, headers_subtitle, headers_text, show_certificate_editor, show_headers_editor, ApplyCertificate, ApplyHeaders, CertificateChoice,
};

/// Shows a failed sign-in (the error, and whether the attempt was a password one).
type ShowFailure = Rc<dyn Fn(&CoreError, bool)>;
/// Runs the sign-in, after confirming first when a re-login would replace local data (the kind,
/// the URL and the username it is about).
type ConfirmOrRun = Rc<dyn Fn(Option<ReplacementKind>, &str, &str)>;

/// The two ways to sign in: a username and password, or an API key (a bearer token made in the
/// server's settings, which names its own user — see `abs_core::accounts::SignIn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    Password,
    ApiKey,
}

pub struct WelcomeScreen {
    pub root: gtk4::Widget,
    #[cfg(test)]
    hooks: TestHooks,
}

#[cfg(test)]
pub struct TestHooks {
    pub landing: gtk4::Image,
    pub url_row: adw::EntryRow,
    pub username_row: adw::EntryRow,
    pub password_row: adw::PasswordEntryRow,
    pub remember_switch: gtk4::Switch,
    pub connect_button: gtk4::Button,
    pub cancel_button: gtk4::Button,
    pub banner: crate::widgets::banner::ErrorBanner,
    pub mode_key: gtk4::ToggleButton,
    pub key_row: adw::EntryRow,
    pub advanced_row: adw::ExpanderRow,
    pub untrusted_switch: gtk4::Switch,
    pub headers_row: adw::ActionRow,
    pub certificate_row: adw::ActionRow,
    pub settings: Rc<std::cell::RefCell<ConnectionSettings>>,
}

#[cfg(test)]
impl WelcomeScreen {
    pub fn test_settings(&self) -> &Rc<std::cell::RefCell<ConnectionSettings>> {
        &self.hooks.settings
    }

    pub fn test_hooks(&self) -> &TestHooks {
        &self.hooks
    }
}

/// Builds the screen. `on_success` fires once, with the (possibly rotated) account, after a
/// successful connect — the caller (`application.rs`) is responsible for swapping window content.
///
/// `previous` turns this into the re-login flow behind Home's "Log in again": the form comes
/// pre-filled with the previous session's URL and username (only the password left to type), a
/// Cancel button (`on_cancel`) returns to the main window, and credentials that would *replace*
/// the previous session's local data — a different account, or a different server — ask for
/// confirmation before `abs_core::accounts::relogin` touches anything.
pub fn build(
    pool: SqlitePool,
    paths: abs_storage::AppPaths,
    previous: Option<ReloginSeed>,
    on_success: impl Fn(AddedAccount) + 'static,
    on_cancel: Option<Rc<dyn Fn()>>,
) -> WelcomeScreen {
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Start)
        .margin_top(48)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .spacing(0)
        .build();

    // Hero: icon, title, subtitle.
    let icon = gtk4::Image::builder()
        .icon_name("network-server-symbolic")
        .pixel_size(40)
        .css_classes(["welcome-icon"])
        .build();
    // Where a screen reader lands when the screen appears (`focus_landing` below): the title
    // read as a heading. On the icon rather than the title label, which GTK won't focus.
    crate::a11y::landing(&icon, &tr("Connect to your Audiobookshelf server"));
    let title = gtk4::Label::builder()
        .label(tr("Connect to your Audiobookshelf server"))
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .css_classes(["title-2"])
        .margin_top(14)
        .build();
    let subtitle = gtk4::Label::builder()
        .label(tr("Enter your server address and sign in to start listening."))
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .css_classes(["dim-label"])
        .margin_top(6)
        .build();
    content.append(&icon);
    content.append(&title);
    content.append(&subtitle);

    // Auth-mode toggle: two linked GtkToggleButtons — AdwToggleGroup is 1.4+ and won't compile
    // under this crate's v1_2 feature ceiling, so this is the hand-rolled equivalent.
    let mode_password = gtk4::ToggleButton::builder().label(tr("Password")).active(true).build();
    let mode_token = gtk4::ToggleButton::builder().label(tr("API Key")).build();
    mode_token.set_group(Some(&mode_password));
    crate::a11y::radio(&mode_password);
    crate::a11y::radio(&mode_token);
    let mode_box = gtk4::Box::builder()
        .css_classes(["linked"])
        .halign(gtk4::Align::Fill)
        .margin_top(22)
        .margin_bottom(16)
        .accessible_role(gtk4::AccessibleRole::RadioGroup)
        .build();
    crate::a11y::name(&mode_box, &tr("Sign-in method"));
    mode_password.set_hexpand(true);
    mode_token.set_hexpand(true);
    mode_box.append(&mode_password);
    mode_box.append(&mode_token);
    content.append(&mode_box);

    // Error banner — hidden until a connect attempt fails.
    let banner = crate::widgets::banner::ErrorBanner::new();
    content.append(banner.widget());

    // Fields, grouped as a libadwaita-1.2-safe ".boxed-list".
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_top(12)
        .build();

    let url_row = adw::EntryRow::builder().title(tr("Server URL")).show_apply_button(false).build();
    crate::widgets::entry_row_input_purpose(&url_row, gtk4::InputPurpose::Url);
    let username_row = adw::EntryRow::builder().title(tr("Username")).show_apply_button(false).build();
    let password_row = adw::PasswordEntryRow::builder().title(tr("Password")).show_apply_button(false).build();
    // On by default: a remembered password is what lets the app sign in again by itself when the
    // server ends the session (see `abs_core::auth`), so one sign-in lasts.
    let remember_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).active(true).build();
    let remember_row = adw::ActionRow::builder()
        .title(tr("Remember password"))
        .subtitle(tr("Signs you in again if the server ends the session. Kept in your keyring."))
        .activatable_widget(&remember_switch)
        .build();
    remember_row.add_suffix(&remember_switch);
    crate::a11y::switch_row(&remember_row, &remember_switch);
    let token_row = adw::EntryRow::builder().title(tr("API Key")).show_apply_button(false).build();
    token_row.set_visible(false);
    // The rows' inner text widgets are what take focus; GTK 4.8 gives them no name of their own.
    for row in [&url_row, &username_row, &token_row] {
        crate::a11y::text_field(row);
    }
    crate::a11y::text_field(&password_row);

    list.append(&url_row);
    list.append(&username_row);
    list.append(&password_row);
    list.append(&remember_row);
    list.append(&token_row);

    // Advanced: what a server behind a private certificate, a proxy or a firewall needs before a
    // sign-in can get through. Chosen here, used for the attempt and saved on the server's row
    // once it succeeds (`ConnectionSettings`); the Connection page edits the same options later.
    let settings: Rc<std::cell::RefCell<ConnectionSettings>> = Rc::new(std::cell::RefCell::new(ConnectionSettings::default()));
    let advanced_row = adw::ExpanderRow::builder()
        .title(tr("Advanced"))
        .subtitle(tr("For servers behind a proxy, a firewall or a private certificate"))
        .build();
    let untrusted_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).build();
    let untrusted_row = adw::ActionRow::builder()
        .title(tr("Allow Untrusted Certificate"))
        .subtitle(tr("Connect to servers with any certificate"))
        .activatable_widget(&untrusted_switch)
        .build();
    untrusted_row.add_suffix(&untrusted_switch);
    crate::a11y::switch_row(&untrusted_row, &untrusted_switch);
    untrusted_switch.connect_state_set({
        let settings = settings.clone();
        move |_, allow| {
            settings.borrow_mut().allow_untrusted_certificate = allow;
            glib::signal::Propagation::Proceed
        }
    });
    let chevron = || gtk4::Image::builder().icon_name("go-next-symbolic").css_classes(["dim-label"]).build();
    let headers_row = adw::ActionRow::builder().title(tr("Custom Headers")).subtitle(headers_subtitle("{}")).activatable(true).build();
    headers_row.add_suffix(&chevron());
    let certificate_row = adw::ActionRow::builder()
        .title(tr("Client Certificate (Experimental)"))
        .subtitle(cert_subtitle(None))
        .subtitle_lines(2)
        .activatable(true)
        .build();
    certificate_row.add_suffix(&chevron());
    advanced_row.add_row(&untrusted_row);
    advanced_row.add_row(&headers_row);
    advanced_row.add_row(&certificate_row);
    // The expander's own header row is what takes focus; its open/closed state goes there.
    if let Some(header) = crate::widgets::find_descendant::<adw::ActionRow>(advanced_row.upcast_ref()) {
        crate::a11y::expanded(&header, advanced_row.is_expanded());
        advanced_row.connect_expanded_notify(move |row| crate::a11y::expanded(&header, row.is_expanded()));
    }
    list.append(&advanced_row);
    content.append(&list);

    let connect_button = gtk4::Button::builder()
        .label(tr("Connect"))
        .css_classes(["suggested-action", "pill"])
        .hexpand(true)
        .sensitive(false)
        .margin_top(22)
        .height_request(44)
        .build();
    content.append(&connect_button);

    // A way back to the (cached) app — the re-login flow ("Log in again") and the Add-Server
    // flow from Settings both provide one; only the true first run has nothing to return to.
    // Without it, "Log in again" would be a trap — a typo'd URL or a changed mind would leave
    // the user stuck on this screen with no route back to their libraries.
    let cancel_button = gtk4::Button::builder()
        .label(tr("Cancel"))
        .css_classes(["pill"])
        .hexpand(true)
        .visible(on_cancel.is_some())
        .margin_top(8)
        .height_request(44)
        .build();
    if let Some(on_cancel) = on_cancel.as_ref() {
        let on_cancel = on_cancel.clone();
        cancel_button.connect_clicked(move |_| on_cancel());
    }
    content.append(&cancel_button);

    // Re-login: pre-fill what the user shouldn't have to retype (URL, username — the session died,
    // not the server address) and put the cursor on the one field that's genuinely empty. The
    // set_text calls fire `changed`, which runs the sensitivity update, so Connect correctly
    // stays disabled until a password is typed.
    if let Some(previous) = &previous {
        url_row.set_text(&previous.url);
        username_row.set_text(&previous.username);
        password_row.grab_focus();
    }

    // The editors are dialogs over the application window, found when a row is activated (this
    // screen can be built before it has a root).
    headers_row.connect_activated({
        let settings = settings.clone();
        move |row| {
            let Some(window) = row.root().and_then(|root| root.downcast::<adw::ApplicationWindow>().ok()) else { return };
            let initial = settings.borrow().custom_headers_json.as_deref().map(headers_text).unwrap_or_default();
            let on_apply: ApplyHeaders = Rc::new({
                let settings = settings.clone();
                let row = row.clone();
                move |json, dialog, _| {
                    let none = abs_core::connection::parse_custom_headers(&json).is_empty();
                    row.set_subtitle(&headers_subtitle(&json));
                    settings.borrow_mut().custom_headers_json = (!none).then_some(json);
                    dialog.close();
                }
            });
            show_headers_editor(&window, &initial, on_apply);
        }
    });
    certificate_row.connect_activated({
        let settings = settings.clone();
        move |row| {
            let Some(window) = row.root().and_then(|root| root.downcast::<adw::ApplicationWindow>().ok()) else { return };
            let current = settings.borrow().client_certificate.as_ref().map(|certificate| certificate.path.to_string_lossy().to_string());
            let on_apply: ApplyCertificate = Rc::new({
                let settings = settings.clone();
                let row = row.clone();
                move |choice, dialog, _| {
                    let certificate = match choice {
                        CertificateChoice::Set { path, password } => Some(ClientCertificate { path, password }),
                        CertificateChoice::Remove => None,
                    };
                    row.set_subtitle(&cert_subtitle(certificate.as_ref().map(|certificate| certificate.path.to_string_lossy().to_string()).as_deref()));
                    settings.borrow_mut().client_certificate = certificate;
                    dialog.close();
                }
            });
            show_certificate_editor(&window, current.as_deref(), on_apply);
        }
    });

    // --- Mode switching: show/hide the fields each mode needs. ---
    let update_visibility: Rc<dyn Fn(AuthMode)> = Rc::new({
        let username_row = username_row.clone();
        let password_row = password_row.clone();
        let remember_row = remember_row.clone();
        let token_row = token_row.clone();
        move |mode: AuthMode| {
            let is_password = mode == AuthMode::Password;
            username_row.set_visible(is_password);
            password_row.set_visible(is_password);
            remember_row.set_visible(is_password);
            token_row.set_visible(!is_password);
        }
    });

    // --- Connect button sensitivity: enabled once the current mode's required fields are filled. ---
    let update_sensitivity: Rc<dyn Fn()> = Rc::new({
        let mode_password = mode_password.clone();
        let url_row = url_row.clone();
        let username_row = username_row.clone();
        let password_row = password_row.clone();
        let token_row = token_row.clone();
        let connect_button = connect_button.clone();
        move || {
            let url_filled = !url_row.text().trim().is_empty();
            let mode_filled = if mode_password.is_active() {
                !username_row.text().trim().is_empty() && !password_row.text().trim().is_empty()
            } else {
                !token_row.text().trim().is_empty()
            };
            connect_button.set_sensitive(url_filled && mode_filled);
        }
    });

    mode_password.connect_toggled({
        let update_visibility = update_visibility.clone();
        let update_sensitivity = update_sensitivity.clone();
        move |btn| {
            if btn.is_active() {
                update_visibility(AuthMode::Password);
                update_sensitivity();
            }
        }
    });
    mode_token.connect_toggled({
        let update_visibility = update_visibility.clone();
        let update_sensitivity = update_sensitivity.clone();
        move |btn| {
            if btn.is_active() {
                update_visibility(AuthMode::ApiKey);
                update_sensitivity();
            }
        }
    });

    for entry in [&url_row, &username_row, &token_row] {
        entry.connect_changed({
            let update_sensitivity = update_sensitivity.clone();
            move |_| update_sensitivity()
        });
    }
    password_row.connect_changed({
        let update_sensitivity = update_sensitivity.clone();
        move |_| update_sensitivity()
    });

    // --- Connect: validate, confirm any replacement, then run the sign-in. ---
    //
    // Failures are shown the same way wherever they happen (the sign-in itself, or the check of
    // whose API key it is before a re-login's confirmation): only a rejected credential tints the
    // credential fields — a connectivity failure isn't a credentials problem — and the failures
    // that Advanced can fix open it, so the remedy is in view.
    let show_failure: ShowFailure = Rc::new({
        let username_row = username_row.clone();
        let password_row = password_row.clone();
        let token_row = token_row.clone();
        let advanced_row = advanced_row.clone();
        let banner = banner.clone();
        move |err, password_mode| {
            if matches!(err, CoreError::Login(abs_api::LoginError::InvalidCredentials)) {
                if password_mode {
                    username_row.add_css_class("error");
                    password_row.add_css_class("error");
                    mark_invalid(&username_row, true, Some(banner.widget().upcast_ref()));
                    mark_invalid(&password_row, true, Some(banner.widget().upcast_ref()));
                } else {
                    token_row.add_css_class("error");
                    mark_invalid(&token_row, true, Some(banner.widget().upcast_ref()));
                }
            }
            if advanced_can_fix(err) {
                advanced_row.set_expanded(true);
            }
            let (title, description) = error_message(err, password_mode);
            banner.set_title(&title);
            banner.set_description(Some(&description));
            banner.set_details(login_details(err).as_deref());
            banner.set_revealed(true);
            // Nothing else has focus by now (the form was insensitive): the failure is spoken.
            banner.focus();
        }
    });

    // The form is locked while a request is out.
    let set_busy: Rc<dyn Fn(bool)> = Rc::new({
        let mode_box = mode_box.clone();
        let list = list.clone();
        let connect_button = connect_button.clone();
        move |busy| {
            mode_box.set_sensitive(!busy);
            list.set_sensitive(!busy);
            connect_button.set_label(&if busy { tr("Connecting…") } else { tr("Connect") });
            // Spoken by a screen reader: the button takes focus (Enter in a field left it
            // elsewhere) and its name changes while it has it; it then goes insensitive and the
            // focus with it, as before. Nothing visible: no focus ring without the keyboard.
            if busy {
                crate::a11y::focus_now(&connect_button);
            }
            crate::a11y::name(&connect_button, &if busy { tr("Connecting…") } else { tr("Connect") });
            connect_button.set_sensitive(!busy);
        }
    });

    // The sign-in run is shared by the direct Connect click and the confirm dialog's "Replace"
    // response: lock the form, call `relogin_with` (re-login flow) or `add_server_and_sign_in`
    // (first run, Add Server), unlock, then fire `on_success` or show the error.
    let run_connect: Rc<dyn Fn()> = Rc::new({
        let pool = pool.clone();
        let paths = paths.clone();
        let previous = previous.clone();
        let url_row = url_row.clone();
        let username_row = username_row.clone();
        let password_row = password_row.clone();
        let token_row = token_row.clone();
        let mode_password = mode_password.clone();
        let remember_switch = remember_switch.clone();
        let settings = settings.clone();
        let set_busy = set_busy.clone();
        let show_failure = show_failure.clone();
        let on_success = Rc::new(on_success);
        move || {
            let url = url_row.text().trim().to_string();
            let password_mode = mode_password.is_active();
            let username = username_row.text().trim().to_string();
            let password = password_row.text().to_string();
            let api_key = token_row.text().trim().to_string();
            let remember = remember_switch.is_active();
            let settings = settings.borrow().clone();

            set_busy(true);

            // Cloned per call: this closure fires once per Connect press (and once per confirmed
            // replacement), so nothing may be moved out of it — widget handles included.
            let pool = pool.clone();
            let paths = paths.clone();
            let previous = previous.clone();
            let on_success = on_success.clone();
            let set_busy = set_busy.clone();
            let show_failure = show_failure.clone();

            // Before the first request: this attempt's own failure logs must already hide them.
            crate::log_privacy::global().register_server_url(&url);
            if password_mode {
                crate::log_privacy::global().register_username(&username);
            }

            glib::spawn_future_local(async move {
                let passwords = crate::password_keyring::store();
                let sign_in = if password_mode { SignIn::Password { username: &username, password: &password } } else { SignIn::ApiKey(&api_key) };
                let result = match &previous {
                    Some(seed) => abs_core::accounts::relogin_with(&pool, &paths, seed, &url, &sign_in, passwords.as_ref(), remember, &settings).await,
                    None => abs_core::accounts::add_server_and_sign_in(&pool, &url, &sign_in, passwords.as_ref(), remember, &settings).await,
                };

                set_busy(false);

                if result.is_ok() {
                    crate::log_privacy::refresh_from_db(&pool).await;
                }

                match result {
                    Ok(added) => on_success(added),
                    Err(err) => show_failure(&err, password_mode),
                }
            });
        }
    });

    // Re-login flow: credentials that would replace the previous session's local data are
    // confirmed before anything runs. The dialog only *previews* `replacement_kind`'s verdict —
    // nothing is persisted until the new sign-in actually succeeds (a failed sign-in leaves the old
    // session intact, so even a confirmed replacement that then fails changes nothing).
    let confirm_or_run: ConfirmOrRun = Rc::new({
        let previous = previous.clone();
        let run_connect = run_connect.clone();
        move |replacement, url, username| {
            let (Some(kind), Some(previous)) = (replacement, previous.as_ref()) else {
                run_connect();
                return;
            };
            let (heading, body) = replacement_warning(&kind, previous, url, username);
            let dialog = gtk4::MessageDialog::builder()
                .message_type(gtk4::MessageType::Warning)
                .text(heading)
                .secondary_text(body)
                .modal(true)
                .build();
            dialog.add_button(&tr("Cancel"), gtk4::ResponseType::Cancel);
            let replace = dialog.add_button(&tr("Replace"), gtk4::ResponseType::Ok);
            replace.add_css_class("destructive-action");
            dialog.connect_response({
                let run_connect = run_connect.clone();
                move |dialog, response| {
                    dialog.destroy();
                    if response == gtk4::ResponseType::Ok {
                        run_connect();
                    }
                }
            });
            dialog.present();
        }
    });

    connect_button.connect_clicked({
        let pool = pool.clone();
        let url_row = url_row.clone();
        let username_row = username_row.clone();
        let password_row = password_row.clone();
        let token_row = token_row.clone();
        let mode_password = mode_password.clone();
        let banner = banner.clone();
        let previous = previous.clone();
        let settings = settings.clone();
        let set_busy = set_busy.clone();
        let show_failure = show_failure.clone();
        let confirm_or_run = confirm_or_run.clone();
        move |_| {
            let password_mode = mode_password.is_active();
            banner.set_revealed(false);
            banner.set_details(None);
            username_row.remove_css_class("error");
            password_row.remove_css_class("error");
            token_row.remove_css_class("error");
            mark_invalid(&username_row, false, None);
            mark_invalid(&password_row, false, None);
            mark_invalid(&token_row, false, None);

            let url = url_row.text().trim().to_string();
            let username = username_row.text().trim().to_string();

            let Some(seed) = previous.clone().filter(|_| !password_mode) else {
                // A password names its user up front, so what a re-login would replace is known now.
                let replacement = previous.as_ref().and_then(|seed| abs_core::accounts::replacement_kind(seed, &url, &username)).filter(|_| password_mode);
                confirm_or_run(replacement, &url, &username);
                return;
            };

            // An API key names no user until the server is asked, and which user decides whether
            // this re-login replaces anything.
            let key = token_row.text().trim().to_string();
            let pool = pool.clone();
            let settings = settings.borrow().clone();
            let set_busy = set_busy.clone();
            let show_failure = show_failure.clone();
            let confirm_or_run = confirm_or_run.clone();
            crate::log_privacy::global().register_server_url(&url);
            set_busy(true);
            glib::spawn_future_local(async move {
                let owner = abs_core::accounts::api_key_username(&pool, &seed, &url, &key, &settings).await;
                set_busy(false);
                match owner {
                    Ok(owner) => {
                        crate::log_privacy::global().register_username(&owner);
                        confirm_or_run(abs_core::accounts::replacement_kind(&seed, &url, &owner), &url, &owner);
                    }
                    Err(err) => show_failure(&err, false),
                }
            });
        }
    });

    // --- Keyboard flow: Enter in the password/token field submits the form, Enter in the other
    // fields advances focus to the next one — the Connect button must not be reachable by pointer
    // only. `entry-activated` fires when Enter is pressed inside an entry row; submission routes
    // through the same production handler as a click (and only when the form is complete, i.e.
    // the button is sensitive — the handler reads the fields itself and has no empty-form guard). ---
    let try_connect: Rc<dyn Fn()> = Rc::new({
        let connect_button = connect_button.clone();
        move || {
            if connect_button.is_sensitive() {
                connect_button.emit_clicked();
            }
        }
    });

    url_row.connect_entry_activated({
        let mode_password = mode_password.clone();
        let username_row = username_row.clone();
        let token_row = token_row.clone();
        move |_| {
            if mode_password.is_active() {
                username_row.grab_focus();
            } else {
                token_row.grab_focus();
            }
        }
    });
    username_row.connect_entry_activated({
        let password_row = password_row.clone();
        move |_| {
            password_row.grab_focus();
        }
    });
    password_row.connect_entry_activated({
        let try_connect = try_connect.clone();
        move |_| try_connect()
    });
    token_row.connect_entry_activated({
        let try_connect = try_connect.clone();
        move |_| try_connect()
    });

    #[cfg(test)]
    let hooks = TestHooks {
        landing: icon.clone(),
        url_row: url_row.clone(),
        username_row: username_row.clone(),
        password_row: password_row.clone(),
        remember_switch: remember_switch.clone(),
        connect_button: connect_button.clone(),
        cancel_button: cancel_button.clone(),
        banner: banner.clone(),
        mode_key: mode_token.clone(),
        key_row: token_row.clone(),
        advanced_row: advanced_row.clone(),
        untrusted_switch: untrusted_switch.clone(),
        headers_row: headers_row.clone(),
        certificate_row: certificate_row.clone(),
        settings: settings.clone(),
    };

    // The whole screen scrolls, so it can get as short as the window does when the on-screen
    // keyboard is up — unscrolled, its minimum height would be the window's while it's shown,
    // and the window never shrinks back afterwards.
    let scroller = gtk4::ScrolledWindow::builder().hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).child(&content).build();
    crate::a11y::focus_landing(&icon, &scroller);

    WelcomeScreen {
        root: scroller.upcast(),
        #[cfg(test)]
        hooks,
    }
}

/// The confirm dialog's copy for a re-login that would replace the previous session's local data,
/// per kind: an account switch on the same server only costs the old account's on-device progress
/// (the server's cache stays valid), while a server switch costs everything cached for the old
/// server. Both stress what the user's *server* keeps — the local data is only a cache.
fn replacement_warning(kind: &ReplacementKind, previous: &ReloginSeed, url: &str, username: &str) -> (String, String) {
    match kind {
        ReplacementKind::SameServerNewAccount => (
            // TRANSLATORS: {username} is the account that was signed in before.
            tr_args("Replace {username} on this server?", &[("username", &previous.username)]),
            // TRANSLATORS: {username} is the account being signed in now; {previous_username} is the account that was signed in before and will be replaced.
            tr_args(
                "Signing in as {username} will replace {previous_username}'s account on this device. Their listening progress stored here will be removed — everything on the server stays where it is.",
                &[("username", username), ("previous_username", &previous.username)],
            ),
        ),
        ReplacementKind::NewServer => (
            tr("Switch server?"),
            // TRANSLATORS: {url} is the address of the new server; {username} is the account that was signed in before; {previous_url} is the address of the old server.
            tr_args(
                "Signing in to {url} will remove {username}'s cached libraries, downloads, and progress from this device. Everything on {previous_url} itself stays untouched.",
                &[("url", url), ("username", &previous.username), ("previous_url", &previous.url)],
            ),
        ),
    }
}

/// Whether the login screen's Advanced section holds the fix for this failure — it then opens by
/// itself.
/// Marks a sign-in field as holding a wrong value for a screen reader, with the banner as the
/// error it can read — on the row's inner text widget, which is what has focus.
fn mark_invalid(row: &impl IsA<adw::EntryRow>, invalid: bool, message: Option<&gtk4::Widget>) {
    if let Some(text) = row.as_ref().delegate() {
        crate::a11y::invalid(&text, invalid, message);
    }
}

fn advanced_can_fix(err: &CoreError) -> bool {
    use abs_api::LoginError as L;
    matches!(err, CoreError::Login(L::Tls(_) | L::ClientCertificate(_) | L::Blocked(_)) | CoreError::ConnectionSetup(_))
}

/// Never clears the fields on failure — the user shouldn't have to retype everything after a
/// typo. Message text is keyed off the error variant, per `docs/design/ui-spec.md`'s error state:
/// what went wrong, then what to do about it. `password_mode` picks the words for a rejected
/// credential. The field tint (see `show_failure`) is keyed off `InvalidCredentials` alone —
/// a connectivity failure isn't a credentials problem.
fn error_message(err: &CoreError, password_mode: bool) -> (String, String) {
    use abs_api::LoginError as L;
    match err {
        CoreError::Login(L::InvalidCredentials) if password_mode => (tr("Unable to sign in"), tr("Check your username and password and try again.")),
        CoreError::Login(L::InvalidCredentials) => (tr("API key not accepted"), tr("Check the key, and that the user it belongs to is active.")),
        CoreError::Login(L::Tls(_)) => (
            tr("Can't verify this server's certificate"),
            tr("If you trust it, turn on “Allow Untrusted Certificate” under Advanced."),
        ),
        CoreError::Login(L::ClientCertificate(_)) => (
            tr("This server wants a client certificate"),
            tr("Choose one under Advanced › Client Certificate. If you already did, the server doesn't accept it."),
        ),
        CoreError::Login(L::Blocked(_)) => (
            tr("The server refused the request"),
            tr("A proxy or firewall in front of it may need a custom header or a client certificate. Add them under Advanced."),
        ),
        CoreError::Login(L::HttpStatus(404)) => (
            tr("No Audiobookshelf server found here"),
            tr("Check the address, including any path such as /audiobookshelf."),
        ),
        CoreError::Login(L::HttpStatus(status)) if (500..600).contains(status) => (
            // TRANSLATORS: {status} is an HTTP status code such as 502.
            tr_args("The server had a problem (HTTP {status})", &[("status", &status.to_string())]),
            tr("Try again in a moment."),
        ),
        CoreError::Login(L::HttpStatus(status)) => (
            // TRANSLATORS: {status} is an HTTP status code such as 418.
            tr_args("Unexpected answer from the server (HTTP {status})", &[("status", &status.to_string())]),
            tr("Check the address and try again."),
        ),
        CoreError::ConnectionSetup(message) => (tr("Can't use the connection settings"), message.clone()),
        CoreError::Login(L::Connect(_) | L::Network(_)) => (tr("Can't reach this server"), tr("Check the address and your connection.")),
        CoreError::Login(L::Timeout(_)) => (tr("This server took too long to respond"), tr("Check your connection and try again.")),
        _ => (tr("Something went wrong"), tr("Please try again.")),
    }
}

/// The raw underlying transport-error text for the banner's "Show details" disclosure — `None`
/// for anything that isn't a low-level transport failure (a bad password or an unexpected server
/// response aren't the kind of opaque library text this disclosure exists for).
fn login_details(err: &CoreError) -> Option<String> {
    match err {
        CoreError::Login(login_err) => login_err.details(),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::{pool, pump_until};
    use std::cell::RefCell;
    use std::time::Duration;

    const DEMO_SERVER_URL: &str = "https://audiobooks.dev/audiobookshelf";

    /// Exercises the real Connect button — `emit_clicked()` invokes the actual production
    /// signal handler, not a simulated input event — against the live public demo server
    /// (confirmed reachable and running server 2.36.0 before this test was written; see
    /// `crates/abs-core/tests/live_demo_server.rs`). This is also what catches a real
    /// GTK-main-loop-vs-Tokio-runtime bridging bug: `add_server_and_login` uses `sqlx`/
    /// `reqwest`, both of which need an entered Tokio runtime context to do any I/O at all —
    /// if `main.rs` didn't keep one entered for the GTK main loop's lifetime, this test would
    /// hang or panic with "no reactor running" instead of completing.
    ///
    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every GTK-touching live-server
    /// scenario in this binary has to run from one single entry point.
    pub(crate) fn run_live(runtime: &tokio::runtime::Runtime) {
        // Success case: correct demo credentials.
        {
            let pool = runtime.block_on(pool());
            let result: Rc<RefCell<Option<AddedAccount>>> = Rc::new(RefCell::new(None));

            let screen = build(
                pool,
                crate::test_support::test_paths(),
                None,
                {
                    let result = result.clone();
                    move |added| *result.borrow_mut() = Some(added)
                },
                None,
            );
            let hooks = screen.test_hooks();

            hooks.url_row.set_text(DEMO_SERVER_URL);
            hooks.username_row.set_text("demo");
            hooks.password_row.set_text("demo");

            assert!(
                hooks.connect_button.is_sensitive(),
                "Connect should be enabled once every required field is filled"
            );
            hooks.connect_button.emit_clicked();

            pump_until(|| result.borrow().is_some(), Duration::from_secs(15));

            let added = result.borrow_mut().take().expect("on_success should have fired");
            assert!(!added.server_id.is_empty());
            assert!(!added.account_id.is_empty());
            assert!(
                !hooks.banner.widget().reveals_child(),
                "the error banner must not be showing after a successful login"
            );
        }

        // Failure case: wrong password against the same live server.
        {
            let pool = runtime.block_on(pool());
            let succeeded = Rc::new(RefCell::new(false));

            let screen = build(
                pool,
                crate::test_support::test_paths(),
                None,
                {
                    let succeeded = succeeded.clone();
                    move |_| *succeeded.borrow_mut() = true
                },
                None,
            );
            let hooks = screen.test_hooks();

            hooks.url_row.set_text(DEMO_SERVER_URL);
            hooks.username_row.set_text("demo");
            hooks.password_row.set_text("definitely-wrong-password");
            hooks.connect_button.emit_clicked();

            pump_until(
                || hooks.banner.widget().reveals_child(),
                Duration::from_secs(15),
            );

            assert!(!*succeeded.borrow(), "on_success must not fire on a failed login");
            assert!(
                hooks.banner.widget().reveals_child(),
                "the error banner should now be visible"
            );
            assert!(
                hooks.username_row.has_css_class("error") && hooks.password_row.has_css_class("error"),
                "a credentials failure should tint the username/password fields"
            );
            assert!(
                !hooks.banner.details_visible(),
                "a bad password isn't a low-level transport error — there's no raw detail to show"
            );
        }
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. What a screen reader gets on this
    /// screen: a heading to land on, fields whose inner text is named after the row, the
    /// sign-in method as radio buttons, one-stop switch rows, the Advanced expander's state,
    /// "Connecting…" spoken from the button, and a failure that takes focus as an alert naming
    /// the invalid fields.
    pub(crate) fn run_screen_reader_support(runtime: &tokio::runtime::Runtime) {
        use crate::a11y::tests::{is_checked, is_expanded, label_of, role_of};
        let pool = runtime.block_on(pool());
        let screen = build(pool, crate::test_support::test_paths(), None, |_| {}, None);
        let hooks = screen.test_hooks();
        let window = gtk4::Window::builder().default_width(360).default_height(720).child(&screen.root).build();
        window.present();
        pump_until(|| hooks.landing.has_focus(), Duration::from_secs(5));
        assert!(hooks.landing.has_focus(), "the screen's heading takes focus on arrival");
        assert_eq!(label_of(&hooks.landing).as_deref(), Some("Connect to your Audiobookshelf server"));
        assert_eq!(role_of(&hooks.landing), gtk4::AccessibleRole::Heading);
        assert!(!window.gets_focus_visible(), "no focus ring for a sighted user");

        for (row, title) in [(&hooks.url_row, "Server URL"), (&hooks.username_row, "Username"), (&hooks.key_row, "API Key")] {
            let text = row.delegate().unwrap();
            assert_eq!(role_of(&text), gtk4::AccessibleRole::TextBox, "{title}");
            assert_eq!(label_of(&text).as_deref(), Some(title));
        }
        assert_eq!(label_of(&hooks.password_row.delegate().unwrap()).as_deref(), Some("Password"));

        assert_eq!(role_of(&hooks.mode_key), gtk4::AccessibleRole::Radio);
        assert!(!is_checked(&hooks.mode_key), "Password is the mode at first");
        hooks.mode_key.set_active(true);
        assert!(is_checked(&hooks.mode_key));
        hooks.mode_key.set_active(false);

        assert!(!hooks.remember_switch.is_focusable(), "the row is the one Tab stop");
        let remember_row = hooks.remember_switch.parent().and_then(|suffixes| suffixes.ancestor(adw::ActionRow::static_type())).unwrap();
        assert_eq!(role_of(&remember_row), gtk4::AccessibleRole::Switch);
        assert!(is_checked(&remember_row));
        hooks.remember_switch.set_active(false);
        assert!(!is_checked(&remember_row));

        let advanced_header = crate::widgets::find_descendant::<adw::ActionRow>(hooks.advanced_row.upcast_ref()).unwrap();
        assert_eq!(advanced_header.title(), "Advanced", "the expander's own header row carries the state");
        assert!(!is_expanded(&advanced_header));
        hooks.advanced_row.set_expanded(true);
        assert!(is_expanded(&advanced_header));

        hooks.url_row.set_text("http://127.0.0.1:1");
        hooks.username_row.set_text("demo");
        hooks.password_row.set_text("demo");
        hooks.connect_button.grab_focus();
        hooks.connect_button.emit_clicked();
        assert_eq!(label_of(&hooks.connect_button).as_deref(), Some("Connecting…"), "spoken before the button loses focus");
        pump_until(|| hooks.banner.widget().reveals_child(), Duration::from_secs(15));
        pump_until(|| hooks.banner.has_focus(), Duration::from_secs(5));
        assert!(hooks.banner.has_focus(), "the failure takes focus, so it is spoken");
        assert_eq!(hooks.banner.accessible_name().as_deref(), Some("Can't reach this server. Check the address and your connection."));
        assert_eq!(label_of(&hooks.connect_button).as_deref(), Some("Connect"));
        assert!(!window.gets_focus_visible());
    }

    /// The re-login flow end to end: pre-fill, the Cancel affordance, and confirm-before-replace.
    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point.
    pub(crate) fn run_relogin_flow(runtime: &tokio::runtime::Runtime) {
        use abs_core::accounts::ReloginSeed;
        use std::cell::Cell;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn find_message_dialog() -> Option<gtk4::MessageDialog> {
            gtk4::Window::list_toplevels()
                .into_iter()
                .find_map(|window| window.downcast::<gtk4::MessageDialog>().ok())
        }

        fn seed_session(
            runtime: &tokio::runtime::Runtime,
            url: &str,
            username: &str,
        ) -> (sqlx::SqlitePool, ReloginSeed) {
            let pool = runtime.block_on(pool());
            let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, url)).unwrap();
            let account_id = runtime
                .block_on(abs_storage::repo::accounts::add(&pool, &server_id, username, "old-token", None))
                .unwrap();
            (
                pool,
                ReloginSeed {
                    server_id,
                    account_id,
                    url: url.to_string(),
                    username: username.to_string(),
                },
            )
        }

        // --- Pre-fill and cancel: the session died, not the server address — the user should
        // only have to type a password, and changing their mind must lead back to the app. ---
        {
            let (pool, previous) = seed_session(runtime, "http://127.0.0.1:1", "jane");
            let cancelled = Rc::new(Cell::new(false));
            let on_cancel = {
                let cancelled = cancelled.clone();
                Rc::new(move || cancelled.set(true)) as Rc<dyn Fn()>
            };
            let screen = build(pool, crate::test_support::test_paths(), Some(previous), |_| {}, Some(on_cancel));
            let hooks = screen.test_hooks();

            assert_eq!(hooks.url_row.text(), "http://127.0.0.1:1", "the URL must come pre-filled");
            assert_eq!(hooks.username_row.text(), "jane", "the username must come pre-filled");
            assert_eq!(hooks.password_row.text(), "", "only the password is left to type");
            assert!(hooks.cancel_button.is_visible(), "a re-login must be abortable");

            hooks.cancel_button.emit_clicked();
            assert!(cancelled.get(), "cancel must hand control back to the shell via on_cancel");
        }

        // --- Same credentials: no confirmation, straight to a token-rotating login. ---
        {
            let mock_server = runtime.block_on(MockServer::start());
            runtime.block_on(
                Mock::given(method("POST"))
                    .and(path("/login"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "user": { "id": "user-1", "username": "jane", "accessToken": "new-access", "refreshToken": "new-refresh" }
                    })))
                    .mount(&mock_server),
            );
            let (pool, previous) = seed_session(runtime, &mock_server.uri(), "jane");

            let succeeded = Rc::new(Cell::new(false));
            let screen = build(
                pool.clone(),
                crate::test_support::test_paths(),
                Some(previous.clone()),
                {
                    let succeeded = succeeded.clone();
                    move |_| succeeded.set(true)
                },
                None,
            );
            let hooks = screen.test_hooks();

            hooks.password_row.set_text("hunter2");
            hooks.connect_button.emit_clicked();

            pump_until(|| succeeded.get(), Duration::from_secs(15));
            let account = runtime
                .block_on(abs_storage::repo::accounts::get(&pool, &previous.account_id));
            assert_eq!(account.unwrap().token, "new-access", "same credentials must rotate the stored tokens in place");
        }

        // --- Different username, dialog cancelled: nothing happens at all. ---
        {
            let mock_server = runtime.block_on(MockServer::start());
            runtime.block_on(
                Mock::given(method("POST"))
                    .and(path("/login"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "user": { "id": "user-2", "username": "bob", "accessToken": "bob-token", "refreshToken": None::<String> }
                    })))
                    .mount(&mock_server),
            );
            let (pool, previous) = seed_session(runtime, &mock_server.uri(), "jane");

            let succeeded = Rc::new(Cell::new(false));
            let screen = build(
                pool.clone(),
                crate::test_support::test_paths(),
                Some(previous.clone()),
                {
                    let succeeded = succeeded.clone();
                    move |_| succeeded.set(true)
                },
                None,
            );
            let hooks = screen.test_hooks();

            hooks.username_row.set_text("bob");
            hooks.password_row.set_text("hunter2");
            hooks.connect_button.emit_clicked();

            pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
            find_message_dialog().unwrap().response(gtk4::ResponseType::Cancel);

            pump_until(|| hooks.connect_button.is_sensitive(), Duration::from_secs(5));
            assert!(!succeeded.get(), "a cancelled replacement must not log in");
            let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &previous.account_id));
            assert_eq!(
                account.unwrap().token,
                "old-token",
                "a cancelled replacement must leave the old session untouched"
            );
        }

        // --- Different username, dialog confirmed: the replacement runs. ---
        {
            let mock_server = runtime.block_on(MockServer::start());
            runtime.block_on(
                Mock::given(method("POST"))
                    .and(path("/login"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "user": { "id": "user-2", "username": "bob", "accessToken": "bob-token", "refreshToken": None::<String> }
                    })))
                    .mount(&mock_server),
            );
            let (pool, previous) = seed_session(runtime, &mock_server.uri(), "jane");

            let succeeded = Rc::new(Cell::new(false));
            let screen = build(
                pool.clone(),
                crate::test_support::test_paths(),
                Some(previous.clone()),
                {
                    let succeeded = succeeded.clone();
                    move |_| succeeded.set(true)
                },
                None,
            );
            let hooks = screen.test_hooks();

            hooks.username_row.set_text("bob");
            hooks.password_row.set_text("hunter2");
            hooks.connect_button.emit_clicked();

            pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
            find_message_dialog().unwrap().response(gtk4::ResponseType::Ok);

            pump_until(|| succeeded.get(), Duration::from_secs(15));
            assert!(
                runtime.block_on(abs_storage::repo::accounts::get(&pool, &previous.account_id)).is_err(),
                "the confirmed replacement must remove the old account"
            );
            let active = runtime.block_on(abs_storage::repo::accounts::get_active(&pool)).unwrap().unwrap();
            assert_eq!(active.username, "bob", "the replacement account must be the active one");
        }
    }

    /// Covers both the disabled/toggling behavior and the connectivity-failure error state. Not a
    /// `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching scenario in
    /// this binary has to run from one single entry point (`gtk4::init()` can only succeed once
    /// per process/thread, and libtest gives every `#[test]` fn its own fresh OS thread even under
    /// `--test-threads=1`, which only limits how many run *concurrently*, not which thread each
    /// runs on).
    pub(crate) fn run(runtime: &tokio::runtime::Runtime) {
        // Starts disabled; filling every required Password-mode field enables Connect.
        {
            let pool = runtime.block_on(pool());
            let screen = build(pool, crate::test_support::test_paths(), None, |_| {}, None);
            let hooks = screen.test_hooks();

            assert!(!hooks.connect_button.is_sensitive(), "empty form should start disabled");
            assert!(hooks.remember_switch.is_active(), "Remember password starts on, so one sign-in lasts");
            let url_text = hooks
                .url_row
                .delegate()
                .unwrap()
                .downcast::<gtk4::Text>()
                .expect("EntryRow delegate should be a GtkText");
            assert_eq!(
                url_text.input_purpose(),
                gtk4::InputPurpose::Url,
                "the Server URL field must ask the on-screen keyboard for its URL layout"
            );

            hooks.url_row.set_text(DEMO_SERVER_URL);
            hooks.username_row.set_text("demo");
            hooks.password_row.set_text("demo");
            assert!(
                hooks.connect_button.is_sensitive(),
                "filling every required Password-mode field should enable Connect"
            );
        }

        // Connectivity failures (nothing listening at the given address) must show a different
        // banner message than a credentials failure, must NOT tint the username/password fields
        // — see docs/design/ui-spec.md's Welcome/Server login error-state section — and, since
        // this is a low-level transport failure, must expose the raw error via the banner's
        // "Show details" disclosure. Uses a real TCP connection attempt to an address nothing
        // listens on rather than wiremock, so this exercises abs_api::LoginError::Connect for
        // real; it's fast (immediate connection refused) and needs no external network, so it
        // isn't #[ignore]d.
        {
            let pool = runtime.block_on(pool());
            let screen = build(pool, crate::test_support::test_paths(), None, |_| {}, None);
            let hooks = screen.test_hooks();

            hooks.url_row.set_text("http://127.0.0.1:1");
            hooks.username_row.set_text("demo");
            hooks.password_row.set_text("demo");
            hooks.connect_button.emit_clicked();

            pump_until(
                || hooks.banner.widget().reveals_child(),
                Duration::from_secs(15),
            );

            assert!(hooks.banner.widget().reveals_child(), "the error banner should now be visible");
            assert_eq!(hooks.banner.title(), "Can't reach this server");
            assert_eq!(hooks.banner.description(), "Check the address and your connection.");
            assert!(
                !hooks.username_row.has_css_class("error") && !hooks.password_row.has_css_class("error"),
                "a connectivity failure must not tint the username/password fields"
            );
            assert!(
                hooks.banner.details_visible(),
                "a connectivity failure should expose the 'Show details' disclosure"
            );
            assert!(
                !hooks.banner.details_text().is_empty(),
                "the details disclosure should carry the raw transport error text"
            );
        }

        // Pressing Enter in the password field must submit the form — exactly the same production
        // handler as a Connect click (see the `try_connect` wiring), simulated here by emitting
        // the row's `entry-activated` signal, which is what the Enter key triggers. Points at the
        // same nothing-listening address, so it stays fast and network-free.
        {
            let pool = runtime.block_on(pool());
            let screen = build(pool, crate::test_support::test_paths(), None, |_| {}, None);
            let hooks = screen.test_hooks();

            hooks.url_row.set_text("http://127.0.0.1:1");
            hooks.username_row.set_text("demo");
            hooks.password_row.set_text("demo");
            assert!(
                hooks.connect_button.is_sensitive(),
                "the form must be complete before Enter-submit can fire"
            );
            hooks
                .password_row
                .emit_by_name::<()>("entry-activated", &[]);

            pump_until(
                || hooks.banner.widget().reveals_child(),
                Duration::from_secs(15),
            );
            assert!(
                hooks.banner.widget().reveals_child(),
                "pressing Enter in the password field should submit the form"
            );
            assert_eq!(hooks.banner.title(), "Can't reach this server");
            assert_eq!(hooks.banner.description(), "Check the address and your connection.");
        }
    }

    /// The messages for each failure: what went wrong, then what to do — and the right words for
    /// a rejected password versus a rejected API key. (Plain functions: no GTK involved.)
    #[test]
    fn every_failure_says_what_to_do_about_it() {
        use abs_api::LoginError as L;
        let login = |err: L| CoreError::Login(err);
        let title = |err: &CoreError, password_mode: bool| error_message(err, password_mode).0;
        let description = |err: &CoreError, password_mode: bool| error_message(err, password_mode).1;

        let rejected = login(L::InvalidCredentials);
        assert_eq!(title(&rejected, true), "Unable to sign in");
        assert_eq!(title(&rejected, false), "API key not accepted");
        assert!(description(&rejected, false).contains("key"));

        let blocked = login(L::Blocked(403));
        assert_eq!(title(&blocked, true), "The server refused the request");
        assert!(description(&blocked, true).contains("Advanced"));
        let certificate = login(L::ClientCertificate(None));
        assert_eq!(title(&certificate, true), "This server wants a client certificate");
        assert!(description(&certificate, true).contains("Advanced"));

        assert_eq!(title(&login(L::HttpStatus(404)), true), "No Audiobookshelf server found here");
        assert!(description(&login(L::HttpStatus(404)), true).contains("/audiobookshelf"));
        assert_eq!(title(&login(L::HttpStatus(502)), true), "The server had a problem (HTTP 502)");
        assert_eq!(title(&login(L::HttpStatus(418)), true), "Unexpected answer from the server (HTTP 418)");

        let setup = CoreError::ConnectionSetup("The client certificate couldn't be opened.".to_string());
        assert_eq!(title(&setup, true), "Can't use the connection settings");
        assert_eq!(description(&setup, true), "The client certificate couldn't be opened.");

        assert_eq!(title(&CoreError::UnexpectedResponse("x".into()), true), "Something went wrong");

        // The ones Advanced can fix open it; the rest don't.
        for err in [&blocked, &certificate, &setup] {
            assert!(advanced_can_fix(err));
        }
        assert!(!advanced_can_fix(&rejected));
        assert!(!advanced_can_fix(&login(L::HttpStatus(404))));
    }

    /// API-key sign-in and the Advanced section through the real Connect button. Not a `#[test]`
    /// itself — see `main.rs`'s `mod tests`.
    pub(crate) fn run_api_key_and_advanced(runtime: &tokio::runtime::Runtime) {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let connect = |hooks: &TestHooks, url: &str, key: &str| {
            hooks.url_row.set_text(url);
            hooks.mode_key.set_active(true);
            hooks.key_row.set_text(key);
            assert!(hooks.connect_button.is_sensitive(), "a URL and a key are all API-key mode needs");
            hooks.connect_button.emit_clicked();
        };

        // --- A key the server accepts: the account is named by /api/me and has nothing to refresh;
        // Advanced starts closed and the password fields are out of the way. ---
        {
            let mock_server = runtime.block_on(MockServer::start());
            runtime.block_on(
                Mock::given(method("GET"))
                    .and(path("/api/me"))
                    .and(header("authorization", "Bearer key-123"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "user-1", "username": "jane", "mediaProgress": [] })))
                    .mount(&mock_server),
            );
            let pool = runtime.block_on(pool());
            let result: Rc<RefCell<Option<AddedAccount>>> = Rc::new(RefCell::new(None));
            let screen = build(pool.clone(), crate::test_support::test_paths(), None, {
                let result = result.clone();
                move |added| *result.borrow_mut() = Some(added)
            }, None);
            let hooks = screen.test_hooks();

            assert_eq!(hooks.mode_key.label().as_deref(), Some("API Key"));
            assert!(!hooks.advanced_row.is_expanded(), "Advanced starts closed");
            connect(hooks, &mock_server.uri(), "key-123");
            assert!(!hooks.username_row.is_visible() && !hooks.password_row.is_visible() && hooks.key_row.is_visible());
            pump_until(|| result.borrow().is_some(), Duration::from_secs(10));

            let added = result.borrow_mut().take().expect("the key was accepted");
            let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &added.account_id)).unwrap();
            assert_eq!((account.username.as_str(), account.token.as_str(), account.refresh_token), ("jane", "key-123", None));
            assert!(!hooks.banner.widget().reveals_child());
        }

        // --- A key the server rejects: the key field is tinted, the words are about the key, and
        // nothing else opens or changes. ---
        {
            let mock_server = runtime.block_on(MockServer::start());
            runtime.block_on(Mock::given(method("GET")).and(path("/api/me")).respond_with(ResponseTemplate::new(401)).mount(&mock_server));
            let pool = runtime.block_on(pool());
            let screen = build(pool.clone(), crate::test_support::test_paths(), None, |_| panic!("a rejected key must not sign in"), None);
            let hooks = screen.test_hooks();
            connect(hooks, &mock_server.uri(), "revoked");
            pump_until(|| hooks.banner.widget().reveals_child(), Duration::from_secs(10));
            assert_eq!(Some(hooks.banner.title().as_str()), Some("API key not accepted"));
            assert!(hooks.key_row.has_css_class("error"));
            assert!(!hooks.advanced_row.is_expanded());
            assert!(runtime.block_on(abs_storage::repo::servers::list(&pool)).unwrap().is_empty(), "a failed sign-in stores nothing");
        }

        // --- A proxy in front refuses: the message points at Advanced, which opens by itself. ---
        {
            let mock_server = runtime.block_on(MockServer::start());
            runtime.block_on(Mock::given(method("GET")).and(path("/api/me")).respond_with(ResponseTemplate::new(403)).mount(&mock_server));
            let pool = runtime.block_on(pool());
            let screen = build(pool, crate::test_support::test_paths(), None, |_| panic!("a blocked request must not sign in"), None);
            let hooks = screen.test_hooks();
            connect(hooks, &mock_server.uri(), "key-123");
            pump_until(|| hooks.banner.widget().reveals_child(), Duration::from_secs(10));
            assert_eq!(Some(hooks.banner.title().as_str()), Some("The server refused the request"));
            assert!(hooks.advanced_row.is_expanded(), "the section that holds the fix opens");
            assert!(!hooks.key_row.has_css_class("error"), "a proxy refusing isn't a bad key");
        }

        // --- What Advanced holds is used for the attempt and saved on the new server's row. ---
        {
            let mock_server = runtime.block_on(MockServer::start());
            runtime.block_on(
                Mock::given(method("GET"))
                    .and(path("/api/me"))
                    .and(header("x-proxy-auth", "s3cret"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "user-1", "username": "jane" })))
                    .mount(&mock_server),
            );
            runtime.block_on(Mock::given(method("GET")).and(path("/api/me")).respond_with(ResponseTemplate::new(403)).mount(&mock_server));
            let pool = runtime.block_on(pool());
            let result: Rc<RefCell<Option<AddedAccount>>> = Rc::new(RefCell::new(None));
            let screen = build(pool.clone(), crate::test_support::test_paths(), None, {
                let result = result.clone();
                move |added| *result.borrow_mut() = Some(added)
            }, None);
            let hooks = screen.test_hooks();
            assert_eq!(hooks.headers_row.title().as_str(), "Custom Headers");
            assert_eq!(hooks.certificate_row.title().as_str(), "Client Certificate (Experimental)");
            hooks.untrusted_switch.set_state(true);
            hooks.untrusted_switch.set_active(true);
            // The header editor needs a window to sit on; apply its result the way its Save does.
            screen.test_settings().borrow_mut().custom_headers_json = Some(abs_core::connection::validate_custom_headers_text("X-Proxy-Auth: s3cret").unwrap());
            connect(hooks, &mock_server.uri(), "key-123");
            pump_until(|| result.borrow().is_some(), Duration::from_secs(10));
            let added = result.borrow_mut().take().expect("the header opened the proxy");
            let row = runtime.block_on(abs_storage::repo::servers::get(&pool, &added.server_id)).unwrap();
            assert!(row.disable_ssl_verify);
            assert_eq!(row.custom_headers_json, r#"{"X-Proxy-Auth":"s3cret"}"#);
        }
    }
}
