//! The page shown instead of the app when its local data can't be opened (see `crate::startup`):
//! what went wrong in plain words, what to do about it, and the raw error under "Details".

use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;

use crate::i18n::{tr, tr_args};
use crate::startup::{InstallKind, ProblemKind, StartupProblem, LATEST_RELEASE_URL};

/// What the page's buttons do. `retry` and `start_over` are offered only when given (and, for
/// `start_over`, only for a damaged database).
pub(crate) struct ProblemActions {
    pub retry: Option<Rc<dyn Fn()>>,
    pub start_over: Option<Rc<dyn Fn()>>,
    pub quit: Rc<dyn Fn()>,
}

pub(crate) struct ProblemPage {
    pub root: gtk4::Widget,
    #[cfg(test)]
    pub hooks: ProblemHooks,
}

#[cfg(test)]
pub(crate) struct ProblemHooks {
    pub page: adw::StatusPage,
    pub buttons: gtk4::Box,
    pub details: gtk4::Label,
}

pub(crate) fn build(problem: &StartupProblem, install: InstallKind, log_dir: Option<&Path>, actions: ProblemActions) -> ProblemPage {
    let title = problem.title();
    let description = problem.description(install);
    // A screen reader lands on the page as an alert reading the whole message (see `a11y`).
    let page = adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .title(&title)
        .description(&description)
        .vexpand(true)
        .accessible_role(gtk4::AccessibleRole::Alert)
        .focusable(true)
        .build();
    crate::a11y::name(&page, &title);
    crate::a11y::describe(&page, &description);

    let buttons = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(12).halign(gtk4::Align::Center).build();
    let button = |label: &str, style: &str, run: Rc<dyn Fn()>| {
        let button = gtk4::Button::builder().label(label).css_classes(["pill", style]).build();
        button.connect_clicked(move |_| run());
        buttons.append(&button);
    };
    if problem.kind == ProblemKind::NeedsNewerApp {
        button(
            &tr("Get the latest version"),
            "suggested-action",
            Rc::new(|| {
                tracing::info!("startup problem: opening the latest release page");
                if let Err(err) = crate::external_apps::launch_default_for_uri(LATEST_RELEASE_URL) {
                    tracing::warn!(%err, "couldn't open the latest release page");
                }
            }),
        );
    }
    if let Some(retry) = actions.retry.filter(|_| problem.can_retry()) {
        button(&tr("Try again"), "suggested-action", retry);
    }
    if let Some(start_over) = actions.start_over.filter(|_| problem.kind == ProblemKind::Damaged) {
        button(&tr("Start over…"), "destructive-action", start_over);
    }
    button(&tr("Quit"), "flat", actions.quit);

    // The raw error and where the log is, for a bug report. Selectable, and copyable in one tap.
    let mut details_text = problem.details.clone();
    if let Some(dir) = log_dir {
        details_text.push_str("\n\n");
        // TRANSLATORS: {path} is the folder holding the app's log files.
        details_text.push_str(&tr_args("Logs: {path}", &[("path", &dir.display().to_string())]));
    }
    let details = gtk4::Label::builder()
        .label(&details_text)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .max_width_chars(1)
        .xalign(0.0)
        .selectable(true)
        .css_classes(["caption", "monospace"])
        .build();
    let copy = gtk4::Button::builder().label(tr("Copy details")).css_classes(["flat"]).halign(gtk4::Align::Start).build();
    copy.connect_clicked({
        let details_text = details_text.clone();
        move |button| button.clipboard().set_text(&details_text)
    });
    let details_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(6).margin_top(6).build();
    details_box.append(&details);
    details_box.append(&copy);
    let expander = gtk4::Expander::builder().label(tr("Details")).child(&details_box).margin_top(12).build();

    let content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).margin_start(12).margin_end(12).build();
    content.append(&buttons);
    content.append(&expander);
    page.set_child(Some(&content));

    let scroller = gtk4::ScrolledWindow::builder().hscrollbar_policy(gtk4::PolicyType::Never).child(&page).build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(crate::application::APP_NAME, "")));
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&header);
    root.append(&scroller);
    crate::a11y::focus_landing(&page, &root);

    ProblemPage {
        root: root.upcast(),
        #[cfg(test)]
        hooks: ProblemHooks { page, buttons, details },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::pump_until;
    use std::cell::Cell;
    use std::time::Duration;

    fn labels(buttons: &gtk4::Box) -> Vec<String> {
        let mut out = Vec::new();
        let mut child = buttons.first_child();
        while let Some(widget) = child {
            if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
                out.push(button.label().unwrap_or_default().to_string());
            }
            child = widget.next_sibling();
        }
        out
    }

    fn actions(retried: &Rc<Cell<bool>>) -> ProblemActions {
        let retried = retried.clone();
        ProblemActions { retry: Some(Rc::new(move || retried.set(true))), start_over: Some(Rc::new(|| {})), quit: Rc::new(|| {}) }
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. Each problem says what it is and
    /// offers only what can help: an update for a too-old app (never "Try again" or "Start
    /// over"), "Start over" only for a damaged database, and the screen-reader landing.
    pub(crate) fn run_each_problem_offers_what_can_help(_runtime: &tokio::runtime::Runtime) {
        let retried = Rc::new(Cell::new(false));
        let newer = StartupProblem::new(ProblemKind::NeedsNewerApp, "migration 12 was previously applied but is missing in the resolved migrations", None);
        let page = build(&newer, InstallKind::Package, Some(Path::new("/logs")), actions(&retried));
        assert_eq!(page.hooks.page.title(), "This version of Audiobooklet is too old for your library");
        assert!(page.hooks.page.description().unwrap_or_default().contains("Update Audiobooklet to the latest version"));
        assert_eq!(labels(&page.hooks.buttons), ["Get the latest version", "Quit"]);
        assert!(page.hooks.details.label().contains("migration 12") && page.hooks.details.label().contains("Logs: /logs"));

        let damaged = StartupProblem::new(ProblemKind::Damaged, "file is not a database", Some("/data/db.sqlite3".into()));
        let page = build(&damaged, InstallKind::AppImage, None, actions(&retried));
        assert_eq!(labels(&page.hooks.buttons), ["Try again", "Start over…", "Quit"]);

        for kind in [ProblemKind::UpgradeFailed, ProblemKind::InUse, ProblemKind::OutOfSpace, ProblemKind::NoAccess, ProblemKind::Other] {
            let page = build(&StartupProblem::new(kind, "x", Some("/data".into())), InstallKind::AppImage, None, actions(&retried));
            assert_eq!(labels(&page.hooks.buttons), ["Try again", "Quit"], "{kind:?} is never offered Start over");
        }

        let window = gtk4::Window::builder().child(&page.root).build();
        window.present();
        pump_until(|| page.hooks.page.has_focus(), Duration::from_secs(5));
        assert!(page.hooks.page.has_focus(), "a screen reader lands on the message");
        assert_eq!(crate::a11y::tests::role_of(&page.hooks.page), gtk4::AccessibleRole::Alert);
        crate::test_support::find_button_with_label(page.hooks.buttons.upcast_ref(), "Try again").unwrap().emit_clicked();
        assert!(retried.get());
        window.close();
    }
}
