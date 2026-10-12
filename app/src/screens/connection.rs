//! The per-server Connection destination — per `docs/design/ui-spec.md`'s "Connection"
//! section. One instance per configured server, pushed from a server row's body (or the
//! Account row, for the active server) in Settings by swapping the window's content — the
//! same mechanism the full player uses, since `AdwNavigationView` is out of reach at this
//! crate's libadwaita `v1_2` ceiling (see `app/Cargo.toml`). The back button restores the
//! captured shell widget, so no shell state is lost.
//!
//! Real: the Server connection group (the full URL as a monospace subtitle — a literal value
//! being confirmed — with an info popover explaining what the connection is used for), the
//! Advanced group (Custom Headers, Disable SSL verification, Client certificate — PKCS#12
//! under this workspace's native-tls backend — Local network server address with its
//! reachability probe, and Change User Agent; each row persists to the server's table row and
//! is picked up by the next connection mint, see `abs_core::connection`), and the destructive
//! Disconnect action, which signs out this server's account (its local progress cascades away;
//! the server and its cache survive) after the same kind of confirmation the other destructive
//! session changes use.

use std::rc::Rc;

use adw::glib;
use adw::prelude::*;
use sqlx::SqlitePool;

use abs_storage::models::{Account, Server};

use crate::i18n::{tr, tr_args};
use crate::screens::settings::{confirm, host_of};
use crate::widgets::connection_editors::{
    add_dialog_content, cert_subtitle, headers_subtitle, headers_text, save_cancel_dialog, show_certificate_editor, show_error, show_headers_editor,
    ApplyCertificate, ApplyHeaders, CertificateChoice,
};
use crate::widgets::find_descendant;

pub struct ConnectionScreen {
    pub root: gtk4::Widget,
    #[cfg(test)]
    pub hooks: ConnectionHooks,
}

#[cfg(test)]
pub struct ConnectionHooks {
    pub url_row: adw::ActionRow,
    pub info_button: gtk4::MenuButton,
    pub back_button: gtk4::Button,
    pub disconnect_button: gtk4::Button,
    pub ssl_switch: gtk4::Switch,
    pub headers_row: adw::ActionRow,
    pub cert_row: adw::ActionRow,
    pub local_address_row: adw::ActionRow,
    pub user_agent_row: adw::ActionRow,
}

pub fn build(
    pool: SqlitePool,
    server: Server,
    account: Option<Account>,
    window: &adw::ApplicationWindow,
    shell_root: &gtk4::Widget,
    on_session_changed: Rc<dyn Fn()>,
) -> ConnectionScreen {
    ensure_mono_css();

    // Declared here so every action below can capture it — this screen previously had no toast
    // surface at all, so a background failure (disconnect, or any of the Advanced editors) had
    // nowhere to report to besides the log.
    let toast_overlay = adw::ToastOverlay::new();

    let back_button = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .css_classes(["flat"])
        .build();
    back_button.connect_clicked({
        let window = window.clone();
        let shell_root = shell_root.clone();
        move |_| crate::widgets::swap_content(&window, &shell_root)
    });

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&tr("Connection"), "")));
    header.pack_start(&back_button);

    let page = adw::PreferencesPage::new();

    let connection_group = adw::PreferencesGroup::new();
    connection_group.set_title(&tr("Server connection"));
    let url_row = adw::ActionRow::builder().title(host_of(&server.url)).subtitle(&server.url).build();
    url_row.add_css_class("mono-subtitle");

    let info_button = gtk4::MenuButton::builder()
        .icon_name("dialog-information-symbolic")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .build();
    let info_label = gtk4::Label::builder()
        .label(tr("This connection is used for syncing, streaming and downloads — every call the app makes to the server goes through it."))
        .wrap(true)
        .max_width_chars(32)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    info_button.set_popover(Some(&gtk4::Popover::builder().child(&info_label).build()));
    url_row.add_suffix(&info_button);
    connection_group.add(&url_row);
    page.add(&connection_group);

    // --- Advanced group: the per-server transport settings, each persisted to the server's row
    // on the spot and picked up by the next connection mint (sync, downloads, playback) —
    // see `abs_core::connection` for how a row becomes real traffic.
    let server_id = server.id.clone();
    let advanced_group = adw::PreferencesGroup::new();
    advanced_group.set_title(&tr("Advanced"));

    let headers_row = adw::ActionRow::builder()
        .title(tr("Custom Headers"))
        .subtitle(headers_subtitle(&server.custom_headers_json))
        .activatable(true)
        .build();
    headers_row.add_suffix(&chevron());
    let headers_json = Rc::new(std::cell::RefCell::new(server.custom_headers_json.clone()));
    headers_row.connect_activated({
        let window = window.clone();
        let pool = pool.clone();
        let server_id = server_id.clone();
        let row = headers_row.clone();
        let headers_json = headers_json.clone();
        move |_| {
            let on_apply: ApplyHeaders = Rc::new({
                let pool = pool.clone();
                let server_id = server_id.clone();
                let row = row.clone();
                let headers_json = headers_json.clone();
                move |json, dialog, error_label| {
                    let pool = pool.clone();
                    let server_id = server_id.clone();
                    let row = row.clone();
                    let headers_json = headers_json.clone();
                    let dialog = dialog.clone();
                    let error_label = error_label.clone();
                    glib::spawn_future_local(async move {
                        if let Err(err) = abs_storage::repo::servers::set_custom_headers_json(&pool, &server_id, &json).await {
                            tracing::warn!(%err, server_id, "couldn't persist the custom headers");
                            show_error(&error_label, &err.to_string());
                            return;
                        }
                        row.set_subtitle(&headers_subtitle(&json));
                        *headers_json.borrow_mut() = json;
                        dialog.close();
                    });
                }
            });
            show_headers_editor(&window, &headers_text(&headers_json.borrow()), on_apply);
        }
    });
    advanced_group.add(&headers_row);

    let ssl_row = adw::ActionRow::builder()
        .title(tr("Disable SSL Verification"))
        .subtitle(tr("Connect to servers with any certificate"))
        .build();
    let ssl_switch = gtk4::Switch::builder().valign(gtk4::Align::Center).state(server.disable_ssl_verify).active(server.disable_ssl_verify).build();
    ssl_switch.connect_state_set({
        let pool = pool.clone();
        let server_id = server_id.clone();
        let toast_overlay = toast_overlay.clone();
        // `state` is what a user toggle drives; `active` is what the next toggle flips from —
        // both initialized (same discipline as the Settings screen's switches).
        move |_, disable| {
            // Optimistic: the switch already reflects the new state. A toast beats leaving a
            // switch that visually flipped but silently didn't persist.
            let pool = pool.clone();
            let server_id = server_id.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                if let Err(err) = abs_storage::repo::servers::set_disable_ssl_verify(&pool, &server_id, disable).await {
                    crate::error_reporting::report_background_error(&toast_overlay, &tr("Saving the SSL verification setting"), err);
                }
            });
            glib::signal::Propagation::Proceed
        }
    });
    ssl_row.add_suffix(&ssl_switch);
    ssl_row.set_activatable_widget(Some(&ssl_switch));
    advanced_group.add(&ssl_row);

    let local_address_row = adw::ActionRow::builder()
        .title(tr("Local Network Server Address"))
        .subtitle(text_subtitle(server.local_network_address.as_deref()))
        .activatable(true)
        .build();
    local_address_row.add_suffix(&chevron());
    local_address_row.connect_activated({
        let window = window.clone();
        let pool = pool.clone();
        let server_id = server_id.clone();
        let row = local_address_row.clone();
        let current = server.local_network_address.clone();
        move |_| {
            let on_save = {
                let pool = pool.clone();
                let server_id = server_id.clone();
                let row = row.clone();
                Rc::new(move |text: &str, dialog: &gtk4::Dialog, error_label: &gtk4::Label| match abs_core::connection::validate_local_address(text) {
                    Err(message) => show_error(error_label, &message),
                    Ok(address) => {
                        let pool = pool.clone();
                        let server_id = server_id.clone();
                        let row = row.clone();
                        let dialog = dialog.clone();
                        let error_label = error_label.clone();
                        glib::spawn_future_local(async move {
                            let address = (!address.is_empty()).then_some(address.as_str());
                            if let Err(err) = abs_storage::repo::servers::set_local_network_address(&pool, &server_id, address).await {
                                tracing::warn!(%err, server_id, "couldn't persist the local network address");
                                show_error(&error_label, &err.to_string());
                                return;
                            }
                            if let Some(address) = address {
                                crate::log_privacy::global().register_address(address);
                            }
                            row.set_subtitle(&text_subtitle(address));
                            dialog.close();
                        });
                    }
                })
            };
            show_entry_dialog(
                &window,
                &tr("Local Network Server Address"),
                &tr("Define server access for home Wi-Fi — used when it's reachable, the public address otherwise."),
                current.as_deref().unwrap_or(""),
                gtk4::InputPurpose::Url,
                on_save,
            );
        }
    });
    advanced_group.add(&local_address_row);

    let user_agent_row = adw::ActionRow::builder()
        .title(tr("Change User Agent"))
        .subtitle(server.user_agent.as_deref().map(str::trim).filter(|user_agent| !user_agent.is_empty()).map(str::to_string).unwrap_or_else(|| tr("Default")))
        .activatable(true)
        .build();
    user_agent_row.add_suffix(&chevron());
    user_agent_row.connect_activated({
        let window = window.clone();
        let pool = pool.clone();
        let server_id = server_id.clone();
        let row = user_agent_row.clone();
        let current = server.user_agent.clone();
        move |_| {
            let on_save = {
                let pool = pool.clone();
                let server_id = server_id.clone();
                let row = row.clone();
                Rc::new(move |text: &str, dialog: &gtk4::Dialog, error_label: &gtk4::Label| {
                    let user_agent = text.trim().to_string();
                    let error_label = error_label.clone();
                    let pool = pool.clone();
                    let server_id = server_id.clone();
                    let row = row.clone();
                    let dialog = dialog.clone();
                    glib::spawn_future_local(async move {
                        let stored = (!user_agent.is_empty()).then_some(user_agent.as_str());
                        if let Err(err) = abs_storage::repo::servers::set_user_agent(&pool, &server_id, stored).await {
                            tracing::warn!(%err, server_id, "couldn't persist the user agent");
                            show_error(&error_label, &err.to_string());
                            return;
                        }
                        row.set_subtitle(&stored.map(str::to_string).unwrap_or_else(|| tr("Default")));
                        dialog.close();
                    });
                })
            };
            show_entry_dialog(
                &window,
                &tr("Change User Agent"),
                &tr("Customize the application User-Agent header. Leave empty for the default."),
                current.as_deref().unwrap_or(""),
                gtk4::InputPurpose::FreeForm,
                on_save,
            );
        }
    });
    advanced_group.add(&user_agent_row);

    // Last on purpose: it is the one setting that doesn't reach every kind of traffic
    // (see `show_cert_dialog`), so it sits apart from the settings that do.
    let cert_row = adw::ActionRow::builder()
        .title(tr("Client Certificate (Experimental)"))
        .subtitle(cert_subtitle(server.client_cert_path.as_deref()))
        .subtitle_lines(2)
        .activatable(true)
        .build();
    cert_row.add_suffix(&chevron());
    let cert_path = Rc::new(std::cell::RefCell::new(server.client_cert_path.clone()));
    cert_row.connect_activated({
        let window = window.clone();
        let pool = pool.clone();
        let server_id = server_id.clone();
        let row = cert_row.clone();
        let cert_path = cert_path.clone();
        move |_| {
            let on_apply: ApplyCertificate = Rc::new({
                let pool = pool.clone();
                let server_id = server_id.clone();
                let row = row.clone();
                let cert_path = cert_path.clone();
                move |choice, dialog, error_label| {
                    let pool = pool.clone();
                    let server_id = server_id.clone();
                    let row = row.clone();
                    let cert_path = cert_path.clone();
                    let dialog = dialog.clone();
                    let error_label = error_label.clone();
                    glib::spawn_future_local(async move {
                        let (path, password) = match choice {
                            CertificateChoice::Set { path, password } => (Some(path.to_string_lossy().to_string()), password),
                            CertificateChoice::Remove => (None, None),
                        };
                        if let Err(err) = abs_storage::repo::servers::set_client_cert_path(&pool, &server_id, path.as_deref()).await {
                            tracing::warn!(%err, server_id, "couldn't persist the client certificate");
                            show_error(&error_label, &err.to_string());
                            return;
                        }
                        if let Err(err) = abs_storage::repo::servers::set_client_cert_password(&pool, &server_id, password.as_deref()).await {
                            tracing::warn!(%err, server_id, "couldn't persist the client certificate's password");
                            show_error(&error_label, &err.to_string());
                            return;
                        }
                        row.set_subtitle(&cert_subtitle(path.as_deref()));
                        *cert_path.borrow_mut() = path;
                        dialog.close();
                    });
                }
            });
            show_certificate_editor(&window, cert_path.borrow().as_deref(), on_apply);
        }
    });
    advanced_group.add(&cert_row);

    page.add(&advanced_group);

    // A destructive action below the groups — vertically separated rather than boxed in a card
    // (the spec's styling note for infrequent page-level actions). Not `flat`: with
    // `destructive-action` that draws the label in the page's own background colour (white on
    // white, found in the 2026-10-12 pass), as `settings.rs`'s Remove Server notes.
    let disconnect_button = gtk4::Button::builder()
        .label(tr("Disconnect from the Server"))
        .css_classes(["destructive-action"])
        .halign(gtk4::Align::Center)
        .margin_top(28)
        .sensitive(account.is_some())
        .build();
    disconnect_button.connect_clicked({
        let window = window.clone();
        let account = account.clone();
        let on_session_changed = on_session_changed.clone();
        let toast_overlay = toast_overlay.clone();
        move |_| {
            let Some(account) = account.clone() else { return };
            let window = window.clone();
            let on_session_changed = on_session_changed.clone();
            let pool = pool.clone();
            let toast_overlay = toast_overlay.clone();
            confirm(
                &window,
                &tr_args("Disconnect from {host}?", &[("host", host_of(&server.url))]),
                // TRANSLATORS: {username} is the account being signed out of this server.
                &tr_args(
                    "{username}'s listening progress and bookmarks stored on this device will be removed — everything on the server stays where it is.",
                    &[("username", &account.username)],
                ),
                &tr("Disconnect"),
                Rc::new(move || {
                    let pool = pool.clone();
                    let account_id = account.id.clone();
                    let on_session_changed = on_session_changed.clone();
                    let toast_overlay = toast_overlay.clone();
                    glib::spawn_future_local(async move {
                        if let Err(err) = abs_core::accounts::sign_out(&pool, &account_id, crate::password_keyring::store().as_ref()).await {
                            crate::error_reporting::report_background_error(&toast_overlay, &tr("Disconnecting"), err);
                            return;
                        }
                        on_session_changed();
                    });
                }),
            );
        }
    });

    let content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    content.append(&header);
    content.append(&page);
    content.append(&disconnect_button);
    toast_overlay.set_child(Some(&content));

    ConnectionScreen {
        root: toast_overlay.clone().upcast(),
        #[cfg(test)]
        hooks: ConnectionHooks {
            url_row,
            info_button,
            back_button,
            disconnect_button,
            ssl_switch,
            headers_row,
            cert_row,
            local_address_row,
            user_agent_row,
        },
    }
}

/// The URL row's monospace subtitle — a one-off stylesheet, added once per process. The
/// selector targets the `AdwActionRow`'s internal subtitle label (CSS node `row`, style class
/// `subtitle`), which can't be styled from the widget API.
static MONO_CSS: std::sync::Once = std::sync::Once::new();
fn ensure_mono_css() {
    MONO_CSS.call_once(|| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data("row.mono-subtitle label.subtitle { font-family: monospace; }");
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("a display for the app's css"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

/// A dim chevron suffix — the mockup's "this row opens an editor" affordance (these rows are
/// plain `AdwActionRow`s, which show no chevron of their own at this libadwaita ceiling).
fn chevron() -> gtk4::Image {
    gtk4::Image::builder().icon_name("go-next-symbolic").css_classes(["dim-label"]).build()
}

/// "None" for an unset text setting, the value otherwise — the shared subtitle shape of the
/// Advanced rows that edit text.
fn text_subtitle(value: Option<&str>) -> String {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    value.map(str::to_string).unwrap_or_else(|| tr("None"))
}

/// The single-line editors' save handlers, which also receive the field's text.
type EntrySaveHandler = Rc<dyn Fn(&str, &gtk4::Dialog, &gtk4::Label)>;

/// The single-line editor shared by the Local Network Server Address and Change User Agent
/// rows — the differences (keyboard type, validation, persistence, subtitle text) all live in
/// the arguments, with `on_save` getting the field's text. `input_purpose` picks the on-screen
/// keyboard layout (URL for addresses, plain text for a User-Agent string).
fn show_entry_dialog(
    window: &adw::ApplicationWindow,
    title: &str,
    description: &str,
    initial: &str,
    input_purpose: gtk4::InputPurpose,
    on_save: EntrySaveHandler,
) {
    let (dialog, error_label) = save_cancel_dialog(window, title, {
        let on_save = on_save.clone();
        Rc::new(move |dialog, error_label| {
            let text = find_descendant::<gtk4::Entry>(dialog.content_area().upcast_ref())
                .map(|entry| entry.text().to_string())
                .unwrap_or_default();
            on_save(&text, dialog, error_label);
        })
    });
    let content = add_dialog_content(&dialog, description);
    content
        .append(&gtk4::Entry::builder().text(initial).input_purpose(input_purpose).hexpand(true).build());
    content.append(&error_label);
    dialog.present();
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    use crate::test_support::pump_until;

    fn find_message_dialog() -> Option<gtk4::MessageDialog> {
        gtk4::Window::list_toplevels()
            .into_iter()
            .find_map(|window| window.downcast::<gtk4::MessageDialog>().ok())
    }

    fn find_dialog() -> Option<gtk4::Dialog> {
        gtk4::Window::list_toplevels()
            .into_iter()
            .find_map(|window| window.downcast::<gtk4::Dialog>().ok())
    }

    /// `find_dialog` + "actually on screen" — a merely-presented dialog can still be unmapped
    /// when `list_toplevels` first reports it, and touching an unmapped widget's text (any
    /// text metrics at all) crashes Pango's font handling in a bare test environment. Same
    /// discipline as the scenarios' `app_window.is_mapped()` checks.
    fn pump_until_dialog_mapped() {
        pump_until(|| find_dialog().is_some_and(|dialog| dialog.is_mapped()), Duration::from_secs(5));
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point.
    pub(crate) fn run_url_info_disconnect_and_back(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, "https://library.example/abs")).unwrap();
        let account_id = runtime
            .block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token", None))
            .unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let shell_root: gtk4::Widget = gtk4::Box::new(gtk4::Orientation::Vertical, 0).upcast();
        app_window.set_content(Some(&shell_root));

        let session_changed = Rc::new(Cell::new(false));
        let on_session_changed: Rc<dyn Fn()> = {
            let session_changed = session_changed.clone();
            let pool = pool.clone();
            let window = app_window.clone();
            Rc::new(move || {
                session_changed.set(true);
                crate::application::show_main_or_welcome(
                    &window,
                    pool.clone(),
                    crate::test_support::test_paths(),
                    abs_core::settings::PlaybackSettings::default(),
                )
            })
        };

        let screen = build(pool.clone(), server, Some(account), &app_window, &shell_root, on_session_changed);
        app_window.set_content(Some(&screen.root));
        // Popovers only open in a mapped window (same as the shell scenario's focus check).
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        // The URL row: the address without its scheme as title (the path kept: two servers on one
        // host differ only there), the full URL as the subtitle (a literal value being confirmed),
        // flagged for the monospace stylesheet.
        assert_eq!(screen.hooks.url_row.title(), "library.example/abs");
        assert_eq!(screen.hooks.url_row.subtitle().as_deref(), Some("https://library.example/abs"));
        assert!(screen.hooks.url_row.has_css_class("mono-subtitle"));

        // The info button opens the popover explaining what this connection is used for.
        screen.hooks.info_button.popup();
        pump_until(
            || screen.hooks.info_button.popover().is_some_and(|p| p.is_visible()),
            Duration::from_secs(5),
        );

        // Back restores the shell exactly as it was — the captured widget, not a rebuild.
        screen.hooks.back_button.emit_clicked();
        pump_until(
            {
                let shell_root = shell_root.clone();
                let app_window = app_window.clone();
                move || app_window.content().as_ref() == Some(&shell_root)
            },
            Duration::from_secs(5),
        );

        // Disconnect: cancelling the confirmation changes nothing; confirming signs the
        // server's account out (the only one — so the shell hands over to Welcome).
        app_window.set_content(Some(&screen.root));
        assert!(
            !screen.hooks.disconnect_button.has_css_class("flat"),
            "flat + destructive-action draws the label invisibly"
        );
        screen.hooks.disconnect_button.emit_clicked();
        pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
        find_message_dialog().unwrap().response(gtk4::ResponseType::Cancel);
        pump_until(|| find_message_dialog().is_none(), Duration::from_secs(5));
        assert!(
            runtime.block_on(abs_storage::repo::accounts::get_active(&pool)).unwrap().is_some(),
            "a cancelled disconnect must leave the session intact"
        );

        screen.hooks.disconnect_button.emit_clicked();
        pump_until(|| find_message_dialog().is_some(), Duration::from_secs(5));
        find_message_dialog().unwrap().response(gtk4::ResponseType::Ok);
        pump_until(|| session_changed.get(), Duration::from_secs(10));
        pump_until(|| find_message_dialog().is_none(), Duration::from_secs(5));
        assert!(
            runtime.block_on(abs_storage::repo::accounts::get_active(&pool)).unwrap().is_none(),
            "a confirmed disconnect must remove the account"
        );
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`, same as the scenario above.
    pub(crate) fn run_advanced_rows_persist_and_edit(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, "https://library.example/abs")).unwrap();
        let account_id = runtime
            .block_on(abs_storage::repo::accounts::add(&pool, &server_id, "jane", "token", None))
            .unwrap();
        runtime.block_on(abs_storage::repo::accounts::set_active(&pool, &account_id)).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();
        let account = runtime.block_on(abs_storage::repo::accounts::get(&pool, &account_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let shell_root: gtk4::Widget = gtk4::Box::new(gtk4::Orientation::Vertical, 0).upcast();
        app_window.set_content(Some(&shell_root));

        let screen = build(pool.clone(), server, Some(account), &app_window, &shell_root, Rc::new(|| {}));
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        // Initial subtitles: nothing is configured on a freshly-added server.
        assert_eq!(screen.hooks.headers_row.subtitle().as_deref(), Some("None"));
        assert_eq!(
            screen.hooks.cert_row.subtitle().as_deref(),
            Some("None\nIncomplete: not used when streaming playback"),
            "the value, then a line saying the setting is incomplete"
        );
        assert_eq!(screen.hooks.cert_row.title().as_str(), "Client Certificate (Experimental)");
        assert_eq!(screen.hooks.cert_row.subtitle_lines(), 2);
        // Last in the Advanced group, after Change User Agent.
        assert!(screen.hooks.cert_row.next_sibling().is_none(), "nothing follows the Client Certificate row");
        assert_eq!(screen.hooks.user_agent_row.next_sibling().as_ref(), Some(screen.hooks.cert_row.upcast_ref::<gtk4::Widget>()));
        assert_eq!(screen.hooks.local_address_row.subtitle().as_deref(), Some("None"));
        assert_eq!(screen.hooks.user_agent_row.subtitle().as_deref(), Some("Default"));
        assert!(!screen.hooks.ssl_switch.state(), "SSL verification starts on (the setting is *disable* verification)");

        // The SSL switch persists immediately (both state and active set, so the next toggle
        // flips from the right value).
        screen.hooks.ssl_switch.set_state(true);
        screen.hooks.ssl_switch.set_active(true);
        pump_until(
            {
                let pool = pool.clone();
                let server_id = server_id.clone();
                move || runtime.block_on(async { abs_storage::repo::servers::get(&pool, &server_id).await.unwrap().disable_ssl_verify })
            },
            Duration::from_secs(5),
        );

        // The Local Network Server Address editor: valid input persists (canonicalized to the
        // stored form) and the subtitle follows.
        adw::prelude::ActionRowExt::activate(&screen.hooks.local_address_row);
        pump_until_dialog_mapped();
        let dialog = find_dialog().unwrap();
        let address_entry = find_descendant::<gtk4::Entry>(dialog.content_area().upcast_ref()).unwrap();
        assert_eq!(
            address_entry.input_purpose(),
            gtk4::InputPurpose::Url,
            "the Local Network Server Address editor must ask the on-screen keyboard for its URL layout"
        );
        address_entry.set_text("  http://192.168.1.50:13378/ ");
        dialog.response(gtk4::ResponseType::Ok);
        pump_until(
            {
                let pool = pool.clone();
                let server_id = server_id.clone();
                move || {
                    runtime.block_on(async {
                        abs_storage::repo::servers::get(&pool, &server_id).await.unwrap().local_network_address
                            == Some("http://192.168.1.50:13378".to_string())
                    })
                }
            },
            Duration::from_secs(5),
        );
        pump_until(|| find_dialog().is_none(), Duration::from_secs(5));
        assert_eq!(screen.hooks.local_address_row.subtitle().as_deref(), Some("http://192.168.1.50:13378"));

        // The Custom Headers editor: valid "Name: Value" lines persist as the canonical JSON —
        // the same bytes the mint later parses — and invalid input is rejected inline (error
        // shown, dialog stays open, nothing persisted).
        adw::prelude::ActionRowExt::activate(&screen.hooks.headers_row);
        pump_until_dialog_mapped();
        let dialog = find_dialog().unwrap();
        let text_view = find_descendant::<gtk4::TextView>(dialog.content_area().upcast_ref()).unwrap();
        text_view.buffer().set_text("X-Auth: secret\nno colon here");
        dialog.response(gtk4::ResponseType::Ok);
        pump_until(|| find_dialog().is_some(), Duration::from_secs(5));
        assert!(find_dialog().is_some(), "the dialog stays open on invalid input");
        assert!(
            runtime.block_on(async { abs_storage::repo::servers::get(&pool, &server_id).await.unwrap().custom_headers_json }) == "{}",
            "invalid headers change nothing"
        );
        assert_eq!(screen.hooks.headers_row.subtitle().as_deref(), Some("None"));

        let dialog = find_dialog().unwrap();
        let text_view = find_descendant::<gtk4::TextView>(dialog.content_area().upcast_ref()).unwrap();
        text_view.buffer().set_text("X-Auth: secret\nZ-Extra: 1");
        dialog.response(gtk4::ResponseType::Ok);
        pump_until(
            {
                let pool = pool.clone();
                let server_id = server_id.clone();
                move || {
                    runtime.block_on(async {
                        abs_storage::repo::servers::get(&pool, &server_id).await.unwrap().custom_headers_json
                    }) == r#"{"X-Auth":"secret","Z-Extra":"1"}"#
                }
            },
            Duration::from_secs(5),
        );
        pump_until(|| find_dialog().is_none(), Duration::from_secs(5));
        assert_eq!(screen.hooks.headers_row.subtitle().as_deref(), Some("2 headers"));

        // The Client Certificate editor: saving with no file chosen is an inline error, not a
        // persist. (A real .p12 can't be produced hermetically — the happy path's
        // validate-then-persist wiring shares `validate_client_cert` with the mint, tested in
        // abs-core; the file chooser itself stays manual, per the test plan.)
        adw::prelude::ActionRowExt::activate(&screen.hooks.cert_row);
        pump_until_dialog_mapped();
        let dialog = find_dialog().unwrap();
        assert_eq!(dialog.title().as_deref(), Some("Client Certificate (Experimental)"));
        let first_label = find_descendant::<gtk4::Label>(dialog.content_area().upcast_ref()).unwrap();
        assert_eq!(first_label.label().as_str(), "Experimental and incomplete");
        assert!(first_label.has_css_class("warning"), "the dialog's first line is the warning");
        dialog.response(gtk4::ResponseType::Ok);
        pump_until(
            || {
                find_dialog()
                    .map(|dialog| {
                        find_descendant::<gtk4::Label>(dialog.content_area().upcast_ref())
                            .map(|label| label.is_visible())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            },
            Duration::from_secs(5),
        );
        assert!(
            runtime.block_on(async { abs_storage::repo::servers::get(&pool, &server_id).await.unwrap().client_cert_path }).is_none(),
            "a save with no chosen file changes nothing"
        );
        dialog.response(gtk4::ResponseType::Cancel);
        pump_until(|| find_dialog().is_none(), Duration::from_secs(5));
        assert!(find_dialog().is_none(), "Cancel must close the editor");

        // The User Agent editor: a value persists (trimmed); clearing it falls back to the
        // default — both reflected in the subtitle.
        adw::prelude::ActionRowExt::activate(&screen.hooks.user_agent_row);
        pump_until_dialog_mapped();
        let dialog = find_dialog().unwrap();
        let user_agent_entry = find_descendant::<gtk4::Entry>(dialog.content_area().upcast_ref()).unwrap();
        assert_eq!(
            user_agent_entry.input_purpose(),
            gtk4::InputPurpose::FreeForm,
            "a User-Agent string is free text, not a URL — it must not get the URL keyboard"
        );
        user_agent_entry.set_text("  MyAgent/1.0 ");
        dialog.response(gtk4::ResponseType::Ok);
        pump_until(
            {
                let pool = pool.clone();
                let server_id = server_id.clone();
                move || {
                    runtime.block_on(async {
                        abs_storage::repo::servers::get(&pool, &server_id).await.unwrap().user_agent.as_deref() == Some("MyAgent/1.0")
                    })
                }
            },
            Duration::from_secs(5),
        );
        pump_until(|| find_dialog().is_none(), Duration::from_secs(5));
        assert_eq!(screen.hooks.user_agent_row.subtitle().as_deref(), Some("MyAgent/1.0"));

        adw::prelude::ActionRowExt::activate(&screen.hooks.user_agent_row);
        pump_until(|| find_dialog().is_some(), Duration::from_secs(5));
        let dialog = find_dialog().unwrap();
        find_descendant::<gtk4::Entry>(dialog.content_area().upcast_ref()).unwrap().set_text("");
        dialog.response(gtk4::ResponseType::Ok);
        pump_until(
            {
                let pool = pool.clone();
                let server_id = server_id.clone();
                move || runtime.block_on(async { abs_storage::repo::servers::get(&pool, &server_id).await.unwrap().user_agent.is_none() })
            },
            Duration::from_secs(5),
        );
        pump_until(|| find_dialog().is_none(), Duration::from_secs(5));
        assert_eq!(screen.hooks.user_agent_row.subtitle().as_deref(), Some("Default"));
    }

    /// The text of the dialog's visible inline error label, if one is showing.
    fn visible_error_text(dialog: &gtk4::Dialog) -> Option<String> {
        fn walk(widget: &gtk4::Widget) -> Option<String> {
            if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
                if label.has_css_class("error") && label.is_visible() && !label.text().is_empty() {
                    return Some(label.text().to_string());
                }
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                if let Some(text) = walk(&current) {
                    return Some(text);
                }
                child = current.next_sibling();
            }
            None
        }
        walk(dialog.content_area().upcast_ref())
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`, same as the scenarios above.
    ///
    /// A persist that fails after validation passed must say so inside the still-open dialog,
    /// not leave a Save button that silently does nothing. The server row is deleted under the
    /// open page, which makes every per-server setter fail for real.
    pub(crate) fn run_editor_persist_failures_show_inline(runtime: &tokio::runtime::Runtime) {
        let pool = runtime.block_on(crate::test_support::pool());
        let server_id = runtime.block_on(abs_storage::repo::servers::add(&pool, "https://library.example/abs")).unwrap();
        let server = runtime.block_on(abs_storage::repo::servers::get(&pool, &server_id)).unwrap();

        let app_window = adw::ApplicationWindow::builder().build();
        let shell_root: gtk4::Widget = gtk4::Box::new(gtk4::Orientation::Vertical, 0).upcast();
        app_window.set_content(Some(&shell_root));

        let screen = build(pool.clone(), server, None, &app_window, &shell_root, Rc::new(|| {}));
        app_window.set_content(Some(&screen.root));
        app_window.present();
        pump_until(|| app_window.is_mapped(), Duration::from_secs(5));

        runtime.block_on(abs_storage::repo::servers::remove(&pool, &server_id)).unwrap();

        // Local Network Server Address: valid input, failed write.
        adw::prelude::ActionRowExt::activate(&screen.hooks.local_address_row);
        pump_until_dialog_mapped();
        let dialog = find_dialog().unwrap();
        find_descendant::<gtk4::Entry>(dialog.content_area().upcast_ref())
            .unwrap()
            .set_text("http://192.168.1.50:13378");
        dialog.response(gtk4::ResponseType::Ok);
        pump_until(|| visible_error_text(&dialog).is_some(), Duration::from_secs(5));
        assert!(visible_error_text(&dialog).is_some(), "a failed address save must show an inline error");
        assert!(find_dialog().is_some(), "the dialog stays open so the user can retry or cancel");
        assert_eq!(screen.hooks.local_address_row.subtitle().as_deref(), Some("None"));
        dialog.response(gtk4::ResponseType::Cancel);
        pump_until(|| find_dialog().is_none(), Duration::from_secs(5));
        assert!(find_dialog().is_none(), "Cancel must close the editor");

        // Custom Headers: valid input, failed write.
        adw::prelude::ActionRowExt::activate(&screen.hooks.headers_row);
        pump_until_dialog_mapped();
        let dialog = find_dialog().unwrap();
        find_descendant::<gtk4::TextView>(dialog.content_area().upcast_ref())
            .unwrap()
            .buffer()
            .set_text("X-Auth: secret");
        dialog.response(gtk4::ResponseType::Ok);
        pump_until(|| visible_error_text(&dialog).is_some(), Duration::from_secs(5));
        assert!(visible_error_text(&dialog).is_some(), "a failed headers save must show an inline error");
        assert!(find_dialog().is_some(), "the dialog stays open so the user can retry or cancel");
        assert_eq!(screen.hooks.headers_row.subtitle().as_deref(), Some("None"));
        dialog.response(gtk4::ResponseType::Cancel);
        pump_until(|| find_dialog().is_none(), Duration::from_secs(5));
        assert!(find_dialog().is_none(), "Cancel must close the editor");
    }
}
