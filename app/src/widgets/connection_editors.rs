//! The editors for the connection options that need more than a switch — custom headers and the
//! client certificate — shared by the Connection page (which saves to a server's row) and the
//! login screen's Advanced section (which keeps the choice until the sign-in succeeds). The
//! dialogs validate and hand the result to an `on_apply` callback along with the dialog and its
//! inline error label; the callback persists (or stores) it and closes the dialog, or shows an
//! error in the label.

use std::rc::Rc;

use adw::prelude::*;

use crate::i18n::{ntr, tr};
use crate::widgets::find_descendant;

/// A save handler: invoked with the dialog and its inline error label.
pub(crate) type SaveHandler = Rc<dyn Fn(&gtk4::Dialog, &gtk4::Label)>;

/// Receives the validated headers (canonical JSON) from [`show_headers_editor`], with the dialog and
/// its error label.
pub(crate) type ApplyHeaders = Rc<dyn Fn(String, &gtk4::Dialog, &gtk4::Label)>;
/// Receives the choice from [`show_certificate_editor`], with the dialog and its error label.
pub(crate) type ApplyCertificate = Rc<dyn Fn(CertificateChoice, &gtk4::Dialog, &gtk4::Label)>;

/// What the certificate editor asks for.
pub(crate) enum CertificateChoice {
    Set { path: std::path::PathBuf, password: Option<String> },
    Remove,
}

pub(crate) fn show_error(error_label: &gtk4::Label, message: &str) {
    error_label.set_text(message);
    error_label.set_visible(true);
}

/// The save/cancel dialog shell the editors share: modal, transient, Cancel + Save, and
/// `on_save` invoked on Save with the dialog and the inline error label (also returned, so the
/// editor's builder can place it in the content) — an editor that validates inline shows the
/// error; a successful save closes the dialog.
pub(crate) fn save_cancel_dialog(window: &adw::ApplicationWindow, title: &str, on_save: SaveHandler) -> (gtk4::Dialog, gtk4::Label) {
    let dialog = gtk4::Dialog::builder().title(title).modal(true).transient_for(window).build();
    dialog.add_button(&tr("Cancel"), gtk4::ResponseType::Cancel);
    dialog.add_button(&tr("Save"), gtk4::ResponseType::Ok);
    // `max_width_chars(1)` caps this label's natural width regardless of `wrap`, so a long
    // validation error can't force this dialog window wider than the screen (see
    // `widgets::banner`'s identical fix for the same reasoning).
    let error_label = gtk4::Label::builder().css_classes(["error"]).wrap(true).max_width_chars(1).visible(false).halign(gtk4::Align::Start).build();
    let response_label = error_label.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk4::ResponseType::Ok {
            on_save(dialog, &response_label);
        } else {
            // A GtkDialog doesn't close itself on a response, so without this Cancel was a
            // dead button and the editor could only be dismissed with Escape.
            dialog.close();
        }
    });
    (dialog, error_label)
}

pub(crate) fn add_dialog_content(dialog: &gtk4::Dialog, description: &str) -> gtk4::Box {
    let content = dialog.content_area();
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_spacing(12);
    let description_label = gtk4::Label::builder().label(description).wrap(true).max_width_chars(36).halign(gtk4::Align::Start).build();
    content.append(&description_label);
    content
}

/// How many headers a `custom_headers_json` resolves to — the Custom Headers row's subtitle (the
/// headers themselves were validated at save time; a corrupt value just reads as fewer).
pub(crate) fn headers_subtitle(custom_headers_json: &str) -> String {
    match abs_core::connection::parse_custom_headers(custom_headers_json).len() {
        0 => tr("None"),
        count => ntr("{count} header", "{count} headers", count as u32),
    }
}

/// A `custom_headers_json` as the editor's text: one `Name: Value` per line.
pub(crate) fn headers_text(custom_headers_json: &str) -> String {
    abs_core::connection::parse_custom_headers(custom_headers_json).iter().map(|(name, value)| format!("{name}: {value}\n")).collect()
}

/// The Client Certificate row's subtitle: the bundle's file name (or "None"), then a line saying
/// the setting is incomplete.
pub(crate) fn cert_subtitle(path: Option<&str>) -> String {
    let value = match path {
        Some(path) => std::path::Path::new(path).file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string()),
        None => tr("None"),
    };
    format!("{value}\n{}", tr("Incomplete: not used when streaming playback"))
}

/// The Custom Headers editor: one `Name: Value` pair per line, starting from `initial_text`
/// (see `abs_core::connection::validate_custom_headers_text`, the single writer of the column).
/// `on_apply` gets the canonical JSON.
pub(crate) fn show_headers_editor(window: &adw::ApplicationWindow, initial_text: &str, on_apply: ApplyHeaders) {
    let (dialog, error_label) = save_cancel_dialog(window, &tr("Custom Headers"), {
        Rc::new(move |dialog, error_label| {
            let text = find_descendant::<gtk4::TextView>(dialog.content_area().upcast_ref())
                .map(|view| view.buffer().text(&view.buffer().start_iter(), &view.buffer().end_iter(), false).to_string())
                .unwrap_or_default();
            match abs_core::connection::validate_custom_headers_text(&text) {
                Ok(json) => on_apply(json, dialog, error_label),
                Err(err) => show_error(error_label, &err.to_string()),
            }
        })
    });
    let content = add_dialog_content(&dialog, &tr("Specify headers for server connections — one \"Name: Value\" pair per line. They're attached to every request to this server."));
    let text_view = gtk4::TextView::builder().wrap_mode(gtk4::WrapMode::WordChar).height_request(120).monospace(true).build();
    // The instructions above the text are its name for a screen reader.
    if let Some(instructions) = crate::widgets::find_descendant::<gtk4::Label>(content.upcast_ref()) {
        crate::a11y::labelled_by(&text_view, &instructions);
    }
    text_view.buffer().set_text(initial_text);
    content.append(&gtk4::ScrolledWindow::builder().child(&text_view).hexpand(true).vexpand(true).build());
    content.append(&error_label);
    dialog.present();
}

/// The Client Certificate editor: a PKCS#12 bundle (`.p12`/`.pfx`) picked from disk plus its
/// export password — validated at save time with the exact same read+parse a connection
/// performs (`abs_core::connection::validate_client_cert`), so a bundle the dialog accepts cannot
/// then fail to load. "Remove Certificate" (offered when `current_path` is set) asks for
/// [`CertificateChoice::Remove`].
pub(crate) fn show_certificate_editor(window: &adw::ApplicationWindow, current_path: Option<&str>, on_apply: ApplyCertificate) {
    let chosen: Rc<std::cell::RefCell<Option<std::path::PathBuf>>> = Rc::new(std::cell::RefCell::new(current_path.map(std::path::PathBuf::from)));

    let (dialog, save_error_label) = save_cancel_dialog(window, &tr("Client Certificate (Experimental)"), {
        let chosen = chosen.clone();
        let on_apply = on_apply.clone();
        Rc::new(move |dialog, error_label| {
            let Some(path) = chosen.borrow().clone() else {
                return show_error(error_label, &tr("Choose a certificate file first."));
            };
            let password = find_descendant::<gtk4::PasswordEntry>(dialog.content_area().upcast_ref())
                .map(|entry| entry.text().to_string())
                .filter(|password| !password.is_empty());
            if let Err(message) = abs_core::connection::validate_client_cert(&path, password.as_deref()) {
                return show_error(error_label, &message);
            }
            on_apply(CertificateChoice::Set { path, password }, dialog, error_label);
        })
    });

    let content = add_dialog_content(
        &dialog,
        &tr("Used for signing in, syncing and downloads, but not for streaming playback: books on this server only play once downloaded. A .p12/.pfx bundle and its export password."),
    );
    let experimental_label = gtk4::Label::builder().label(tr("Experimental and incomplete")).css_classes(["warning", "heading"]).halign(gtk4::Align::Start).build();
    content.insert_child_after(&experimental_label, None::<&gtk4::Widget>);
    let file_label = gtk4::Label::new(Some(&current_path.map(|path| path.to_string()).unwrap_or_else(|| tr("No file chosen"))));
    file_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let choose_button = gtk4::Button::builder().label(tr("Choose File…")).halign(gtk4::Align::Start).build();
    choose_button.connect_clicked({
        let window = window.clone();
        let file_label = file_label.clone();
        let chosen = chosen.clone();
        move |_| {
            let chooser = gtk4::FileChooserNative::builder().title(tr("Choose a certificate")).modal(true).transient_for(&window).build();
            let file_filter = gtk4::FileFilter::new();
            file_filter.set_name(Some(&tr("PKCS#12 certificate")));
            file_filter.add_pattern("*.p12");
            file_filter.add_pattern("*.pfx");
            chooser.add_filter(&file_filter);
            chooser.connect_response({
                let file_label = file_label.clone();
                let chosen = chosen.clone();
                move |chooser, response| {
                    if response == gtk4::ResponseType::Ok {
                        if let Some(file) = chooser.file().and_then(|file| file.path()) {
                            file_label.set_text(&file.to_string_lossy());
                            *chosen.borrow_mut() = Some(file);
                        }
                    }
                }
            });
            chooser.show();
        }
    });
    content.append(&file_label);
    content.append(&choose_button);
    content.append(&gtk4::PasswordEntry::builder().show_peek_icon(true).placeholder_text(tr("Export password")).build());
    // Reuses `save_cancel_dialog`'s own error label rather than appending a second, disconnected
    // one — Remove's own failures show here too.
    content.append(&save_error_label);

    if current_path.is_some() {
        let remove_button = gtk4::Button::builder().label(tr("Remove Certificate")).css_classes(["destructive-action"]).halign(gtk4::Align::Start).build();
        remove_button.connect_clicked({
            let dialog = dialog.clone();
            let error_label = save_error_label.clone();
            move |_| on_apply(CertificateChoice::Remove, &dialog, &error_label)
        });
        content.append(&remove_button);
    }
    dialog.present();
}
