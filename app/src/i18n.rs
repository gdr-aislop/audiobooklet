//! Translation support — thin helpers over gettext so every user-visible string in the app goes
//! through one of four functions, and `scripts/update-pot.sh` can extract them with `xgettext`.
//!
//! * [`tr`] — a plain string: `tr("Settings")`.
//! * [`tr_args`] — a string with named placeholders: `tr_args("Sign in to {server}", &[("server", name)])`.
//!   Placeholders are *named* (never `{}`) so a translation can reorder them freely.
//! * [`ntr`] / [`ntr_args`] — plural forms: `ntr_args("{count} chapter", "{count} chapters", n, &[])`.
//!   `{count}` is filled in automatically with `n`; extra named placeholders go in `args`. The
//!   *language's* plural rules decide which form is used (so don't special-case `n == 1` yourself).
//! * [`tr_noop`] — marks a literal for extraction *without* translating it yet, for `&'static str`
//!   tables (`const`s, `match` arms returning text) that are translated where they are shown:
//!   `let (headline, _) = (tr_noop("Connection lost"), …);` … `label.set_label(&tr(headline))`.
//! * [`tr_ctx`] — a string that needs disambiguating context: `tr_ctx("Playback speed", "Speed")`.
//!   The context goes to translators; it is not shown.
//!
//! **Rules for msgids** (the text passed to these functions): write each one as a plain `"…"`
//! literal directly inside the call (no `concat!`, no raw strings, and not through a variable or a
//! function parameter — `xgettext` only sees literals), and put translator notes in a
//! `// TRANSLATORS:` comment on the line(s) directly above the call. `scripts/update-pot.sh`
//! rewrites each source file into something `xgettext` can read (`scripts/rs-for-xgettext.py`),
//! which also understands Rust's `\`-newline string continuation. Strings that are *not* for
//! translation (log lines, CSS classes, icon names, action names, setting keys) don't go through
//! these functions at all.
//!
//! # Choosing the language
//!
//! The text domain is bound once at startup by [`init`], which is given the stored language
//! setting: [`SYSTEM`] (the default) or a language code. "System" asks the user's locale
//! preferences (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`, via GLib) for the first language that
//! has a catalog, and falls back to English — the language the source strings are written in,
//! which needs no catalog. An explicit code overrides that by pointing `LANGUAGE` at it before GTK
//! starts, so libadwaita's own strings follow. The available languages are discovered from the
//! catalogs on disk ([`available_languages`]), so shipping a new `.mo` is all it takes to offer a
//! language. A language change only takes effect at the next start: screens already built keep the
//! text they were given.
//!
//! Until [`init`] runs (and in unit tests, which never call it) gettext returns the msgid
//! unchanged, so the English source text *is* the fallback.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gettextrs::LocaleCategory;

/// gettext domain — the `.mo` files are `<localedir>/<lang>/LC_MESSAGES/audiobooklet.mo`.
pub const DOMAIN: &str = "audiobooklet";

/// The language the source strings are written in; always available, needs no catalog.
pub const SOURCE_LANGUAGE: &str = "en";

/// The setting value meaning "follow the system's language".
pub const SYSTEM: &str = abs_core::settings::LANGUAGE_SYSTEM;

/// What [`init`] decided, for the Settings screen to show.
struct Selection {
    /// The language "System" resolves to on this device.
    system: String,
    /// The language the UI is actually in for this run.
    active: String,
}

static SELECTION: Mutex<Option<Selection>> = Mutex::new(None);

/// The stored choice: [`SYSTEM`] or a language code. Kept apart from [`SELECTION`] because the
/// user changes it while the app runs, and because it exists (as [`SYSTEM`]) before [`init`].
static SETTING: Mutex<Option<String>> = Mutex::new(None);

/// Binds the text domain and selects the language for this run from the stored `setting`
/// ([`SYSTEM`] or a language code). Call once, after the setting is loaded and before GTK is
/// initialised (GTK reads the same locale). Never fails: a missing locale directory or `.mo` file
/// just leaves the app in English.
pub fn init(setting: &str) {
    // `setlocale(LC_ALL, "")` honours LC_ALL / LC_MESSAGES / LANG / LANGUAGE. A failure here
    // (e.g. a locale the system hasn't generated) leaves the "C" locale, i.e. English.
    // SAFETY: `setlocale` modifies process-wide locale state and reads the environment without
    // synchronisation, so it must not run while another thread reads or writes either. This runs
    // at startup, on the main thread, once, before GTK starts and before GStreamer's initialisation
    // thread (`gst_ready::start` comes after the last `init`: a test in `main.rs` keeps it so). The
    // threads that already exist then (the Tokio runtime's workers, the log writer) are idle and
    // never touch the environment or the locale. `init` is not called again after startup.
    unsafe {
        gettextrs::setlocale(LocaleCategory::LcAll, "");
    }
    let dir = locale_dir();
    let available = available_languages_in(&dir);

    // Must be read before LANGUAGE is overridden below: it is what the *system* asks for.
    let preferred: Vec<String> = gtk4::glib::language_names().iter().map(|name| name.to_string()).collect();
    let system = pick_language(&preferred, &available);
    let active = resolve(setting, &system, &available);
    if setting != SYSTEM && active == setting {
        // GNU gettext (and GTK) read LANGUAGE on every lookup; setting it here, before any lookup
        // and before GTK starts, makes the override apply to GTK/libadwaita's own strings too.
        // Only effective under a real (non-"C") locale — gettext ignores LANGUAGE under "C".
        std::env::set_var("LANGUAGE", &active);
    }
    tracing::info!(%setting, %system, %active, ?preferred, ?available, "language selected");
    *SELECTION.lock().unwrap() = Some(Selection { system, active });
    set_selected_setting(setting);

    match gettextrs::bindtextdomain(DOMAIN, &dir) {
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(%err, dir = %dir.display(), "couldn't bind the translations directory; the UI stays in English");
            return;
        }
    }
    if let Err(err) = gettextrs::bind_textdomain_codeset(DOMAIN, "UTF-8") {
        tracing::warn!(%err, "couldn't set the translations' codeset to UTF-8");
    }
    if let Err(err) = gettextrs::textdomain(DOMAIN) {
        tracing::warn!(%err, "couldn't select the translations' text domain; the UI stays in English");
    }
    tracing::info!(dir = %dir.display(), "translations bound");
}

/// The stored language choice ([`SYSTEM`] or a code) — what the Settings row shows as selected.
pub fn selected_setting() -> String {
    SETTING.lock().unwrap().clone().unwrap_or_else(|| SYSTEM.to_string())
}

/// Records a choice the user just made, so reopening Settings shows it. Does not change the
/// language of the running app.
pub fn set_selected_setting(setting: &str) {
    *SETTING.lock().unwrap() = Some(setting.to_string());
}

/// The language "System" resolves to on this device (English when nothing matches).
pub fn system_language() -> String {
    SELECTION.lock().unwrap().as_ref().map_or_else(|| SOURCE_LANGUAGE.to_string(), |s| s.system.clone())
}

/// The language the UI is in for this run.
pub fn active_language() -> String {
    SELECTION.lock().unwrap().as_ref().map_or_else(|| SOURCE_LANGUAGE.to_string(), |s| s.active.clone())
}

/// The language `setting` would give on the next start.
pub fn language_for_setting(setting: &str) -> String {
    resolve(setting, &system_language(), &available_languages())
}

/// Language codes the UI can be shown in: English first, then every language with a catalog on
/// disk, ordered by their own names.
pub fn available_languages() -> Vec<String> {
    available_languages_in(&locale_dir())
}

fn available_languages_in(dir: &Path) -> Vec<String> {
    let mut others: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("LC_MESSAGES").join(format!("{DOMAIN}.mo")).is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        // Directory names like `en.utf8` are locale aliases, not language codes.
        .filter(|code| code != SOURCE_LANGUAGE && !code.contains('.') && !code.is_empty())
        .collect();
    others.sort_by_key(|code| language_name(code).to_lowercase());
    std::iter::once(SOURCE_LANGUAGE.to_string()).chain(others).collect()
}

/// The language a `setting` selects: an explicit code that has a catalog (or is English), else the
/// `system` language.
fn resolve(setting: &str, system: &str, available: &[String]) -> String {
    if setting != SYSTEM && available.iter().any(|code| code == setting) {
        setting.to_string()
    } else {
        system.to_string()
    }
}

/// The first of the user's `preferred` locales (`de_DE.UTF-8`, `de_DE`, `de`, `C`, as GLib lists
/// them) that one of the `available` languages serves: the exact locale, then the same language
/// without its territory, then any available variant of that language (`pt` → `pt_BR`).
/// English when nothing matches.
fn pick_language(preferred: &[String], available: &[String]) -> String {
    let language_part = |code: &str| code.split(['_', '@']).next().unwrap_or(code).to_string();
    for locale in preferred {
        let locale = locale.split('.').next().unwrap_or(locale);
        if locale.is_empty() || locale == "C" || locale == "POSIX" {
            continue;
        }
        let without_modifier = locale.split('@').next().unwrap_or(locale);
        let language = language_part(locale);
        // `sr_RS@latin` → `sr_RS@latin`, `sr_RS`, `sr@latin`, `sr`.
        let language_with_modifier = locale.split_once('@').map(|(_, modifier)| format!("{language}@{modifier}"));
        let exact = [Some(locale), Some(without_modifier), language_with_modifier.as_deref(), Some(language.as_str())];
        let exact = exact.into_iter().flatten();
        for candidate in exact {
            if let Some(hit) = available.iter().find(|code| code.as_str() == candidate) {
                return hit.clone();
            }
        }
        if let Some(hit) = available.iter().find(|code| language_part(code) == language) {
            return hit.clone();
        }
    }
    SOURCE_LANGUAGE.to_string()
}

/// A language's name in that language, for the language picker. Deliberately *not* translated: a
/// reader who can't make out the current UI language must still be able to find their own.
/// Codes without an entry are shown as the code.
pub fn language_name(code: &str) -> String {
    let name = match code {
        "ar" => "العربية",
        "bg" => "Български",
        "ca" => "Català",
        "cs" => "Čeština",
        "da" => "Dansk",
        "de" => "Deutsch",
        "el" => "Ελληνικά",
        "en" => "English",
        "es" => "Español",
        "et" => "Eesti",
        "eu" => "Euskara",
        "fa" => "فارسی",
        "fi" => "Suomi",
        "fr" => "Français",
        "gl" => "Galego",
        "he" => "עברית",
        "hi" => "हिन्दी",
        "hr" => "Hrvatski",
        "hu" => "Magyar",
        "id" => "Bahasa Indonesia",
        "it" => "Italiano",
        "ja" => "日本語",
        "ko" => "한국어",
        "lt" => "Lietuvių",
        "lv" => "Latviešu",
        "nb" => "Norsk bokmål",
        "nl" => "Nederlands",
        "nn" => "Norsk nynorsk",
        "pl" => "Polski",
        "pt" => "Português",
        "pt_BR" => "Português (Brasil)",
        "ro" => "Română",
        "ru" => "Русский",
        "sk" => "Slovenčina",
        "sl" => "Slovenščina",
        "sr" => "Српски",
        "sv" => "Svenska",
        "tr" => "Türkçe",
        "uk" => "Українська",
        "vi" => "Tiếng Việt",
        "zh_CN" => "简体中文",
        "zh_TW" => "繁體中文",
        other => other,
    };
    name.to_string()
}

/// Where the compiled `.mo` files live, first match wins:
/// 1. `$ABS_LOCALEDIR` — for development (`scripts/build-l10n.sh` fills `<out>/locale`) and tests;
/// 2. `<exe dir>/../share/locale` — a relocatable layout (the AppImage's `usr/bin` → `usr/share`,
///    Flatpak's `/app/bin` → `/app/share`);
/// 3. `/usr/share/locale` — a system-wide install (the `.deb`).
fn locale_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ABS_LOCALEDIR").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|bin| bin.join("../share/locale")))
        .filter(|dir| dir.is_dir())
    {
        return dir;
    }
    "/usr/share/locale".into()
}

/// Translates `msgid`.
pub fn tr(msgid: &str) -> String {
    let text = gettextrs::gettext(msgid);
    #[cfg(test)]
    audit::record(&text);
    text
}

/// Marks `msgid` for extraction and returns it unchanged; the caller passes it through [`tr`] at
/// the point it is shown. Only for text that has to live in a `&'static str`.
pub const fn tr_noop(msgid: &'static str) -> &'static str {
    msgid
}

/// Translates `msgid` as a string of the given disambiguating `context`.
pub fn tr_ctx(context: &str, msgid: &str) -> String {
    let text = gettextrs::pgettext(context, msgid);
    #[cfg(test)]
    audit::record(&text);
    text
}

/// Translates `msgid`, then substitutes each `{name}` with its value from `args`.
pub fn tr_args(msgid: &str, args: &[(&str, &str)]) -> String {
    let text = substitute(&tr(msgid), args);
    #[cfg(test)]
    audit::record(&text);
    text
}

/// Plural-aware translation: picks the form `n` calls for in the user's language. `{count}` in the
/// result is replaced with `n`.
pub fn ntr(singular: &str, plural: &str, n: u32) -> String {
    ntr_args(singular, plural, n, &[])
}

/// [`ntr`] plus extra named placeholders.
pub fn ntr_args(singular: &str, plural: &str, n: u32, args: &[(&str, &str)]) -> String {
    let count = n.to_string();
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push(("count", count.as_str()));
    all.extend_from_slice(args);
    let text = substitute(&gettextrs::ngettext(singular, plural, n), &all);
    #[cfg(test)]
    audit::record(&text);
    text
}

/// Replaces every `{name}` in `text` with the matching value, in one left-to-right pass — so a
/// value that itself contains `{…}` (a book titled "{count}") is never expanded a second time. A
/// placeholder with no value is left as written, so a bad translation shows `{typo}` rather than
/// panicking.
fn substitute(text: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        let hit = tail
            .find('}')
            .and_then(|close| args.iter().find(|(name, _)| *name == &tail[1..close]).map(|(_, v)| (close, *v)));
        match hit {
            Some((close, value)) => {
                out.push_str(value);
                rest = &tail[close + 1..];
            }
            None => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Test-only audit for finding UI text that never went through `tr*()` — see "Finding missed
/// strings" in `docs/i18n.md`. Every string the helpers return is remembered; while a UI scenario
/// runs, [`audit::start`] samples all on-screen label/button/tooltip text and, at [`audit::finish`],
/// appends the texts that were *not* produced by a helper to the file named by
/// `ABS_TEST_I18N_AUDIT`. What is left is user data (titles, server names) or a missed string.
#[cfg(test)]
pub(crate) mod audit {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use gtk4::prelude::*;

    static PSEUDO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static TRANSLATED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    static ON_SCREEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);

    pub(super) fn record(text: &str) {
        TRANSLATED.lock().unwrap().get_or_insert_with(HashSet::new).insert(text.to_string());
        // Pseudo-locale mode: every helper result must come out bracketed `[!! … !!]`; one that
        // doesn't was not found in the catalog, i.e. the extracted msgid differs from the literal
        // the program looks up (or the .pot is stale).
        if PSEUDO.load(std::sync::atomic::Ordering::Relaxed) && !text.contains("[!!") {
            append_line(&format!("MISS\t{}", text.replace('\n', "\\n")));
        }
    }

    fn append_line(line: &str) {
        use std::io::Write;
        if let Some(path) = std::env::var_os("ABS_TEST_I18N_AUDIT") {
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    fn collect(widget: &gtk4::Widget, into: &mut HashSet<String>) {
        let mut add = |text: String| {
            if !text.trim().is_empty() {
                into.insert(text);
            }
        };
        if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
            add(label.text().to_string());
        }
        if let Some(button) = widget.downcast_ref::<gtk4::Button>() {
            if let Some(text) = button.label() {
                add(text.to_string());
            }
        }
        if let Some(entry) = widget.downcast_ref::<gtk4::Entry>() {
            if let Some(text) = entry.placeholder_text() {
                add(text.to_string());
            }
        }
        if let Some(text) = widget.tooltip_text() {
            add(text.to_string());
        }
        if let Some(window) = widget.downcast_ref::<gtk4::Window>() {
            if let Some(title) = window.title() {
                add(title.to_string());
            }
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            collect(&c, into);
            child = c.next_sibling();
        }
    }

    fn sample() {
        let mut found = HashSet::new();
        for window in gtk4::Window::list_toplevels() {
            collect(&window, &mut found);
        }
        ON_SCREEN.lock().unwrap().get_or_insert_with(HashSet::new).extend(found);
    }

    /// Starts sampling the open windows every 20 ms when `ABS_TEST_I18N_AUDIT` is set. With
    /// `ABS_TEST_I18N_PSEUDO` also set, the scenario runs under the catalog in `ABS_LOCALEDIR`
    /// (see `scripts/pseudo-locale.py`) and `MISS` lines report helper results absent from it.
    pub(crate) fn start() {
        if std::env::var_os("ABS_TEST_I18N_AUDIT").is_some() {
            if std::env::var_os("ABS_TEST_I18N_PSEUDO").is_some() {
                super::init(super::SYSTEM);
                PSEUDO.store(true, std::sync::atomic::Ordering::Relaxed);
            } else if let Ok(language) = std::env::var("ABS_TEST_I18N_LANGUAGE") {
                // Run the scenario as if this language were chosen in Settings.
                super::init(&language);
            }
            gtk4::glib::timeout_add_local(std::time::Duration::from_millis(20), || {
                sample();
                gtk4::glib::ControlFlow::Continue
            });
        }
    }

    /// Appends this scenario's untranslated on-screen texts (one per line) to the audit file.
    pub(crate) fn finish(scenario: &str) {
        let Some(path) = std::env::var_os("ABS_TEST_I18N_AUDIT") else { return };
        sample();
        let translated = TRANSLATED.lock().unwrap().clone().unwrap_or_default();
        let on_screen = ON_SCREEN.lock().unwrap().clone().unwrap_or_default();
        let mut lines: Vec<_> = on_screen.difference(&translated).map(|t| format!("{}\t{scenario}", t.replace('\n', "\\n"))).collect();
        lines.sort();
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{}", lines.join("\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn system_language_is_the_first_preference_with_a_catalog() {
        let available = codes(&["en", "de", "pt_BR"]);
        assert_eq!(pick_language(&codes(&["de_DE.UTF-8", "de_DE", "de", "C"]), &available), "de");
        assert_eq!(pick_language(&codes(&["fr_FR.UTF-8", "fr_FR", "fr", "de", "C"]), &available), "de", "later preferences are tried");
        assert_eq!(pick_language(&codes(&["pt_PT", "pt"]), &available), "pt_BR", "a variant of the same language serves it");
        assert_eq!(pick_language(&codes(&["pt_BR.UTF-8", "pt_BR", "pt"]), &available), "pt_BR");
        assert_eq!(pick_language(&codes(&["en_GB.UTF-8", "en_GB", "en", "de"]), &available), "en", "English listed first wins");
    }

    #[test]
    fn system_language_falls_back_to_english() {
        let available = codes(&["en", "de"]);
        assert_eq!(pick_language(&codes(&["ja_JP.UTF-8", "ja", "C"]), &available), "en");
        assert_eq!(pick_language(&codes(&["C"]), &available), "en");
        assert_eq!(pick_language(&[], &available), "en");
        assert_eq!(pick_language(&codes(&["de"]), &codes(&["en"])), "en", "no catalogs at all");
    }

    #[test]
    fn locale_modifiers_are_matched_then_dropped() {
        let available = codes(&["en", "sr@latin", "sr"]);
        assert_eq!(pick_language(&codes(&["sr_RS@latin"]), &available), "sr@latin");
        assert_eq!(pick_language(&codes(&["sr_RS"]), &available), "sr");
    }

    #[test]
    fn an_explicit_choice_wins_only_when_it_has_a_catalog() {
        let available = codes(&["en", "de"]);
        assert_eq!(resolve(SYSTEM, "de", &available), "de");
        assert_eq!(resolve("en", "de", &available), "en", "English can be forced on a German system");
        assert_eq!(resolve("de", "en", &available), "de");
        assert_eq!(resolve("fr", "de", &available), "de", "a language that lost its catalog falls back to the system's");
    }

    #[test]
    fn available_languages_come_from_the_catalogs_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(available_languages_in(tmp.path()), ["en"], "English alone when there are no catalogs");
        for code in ["fr", "de", "pt_BR"] {
            let dir = tmp.path().join(code).join("LC_MESSAGES");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("audiobooklet.mo"), b"").unwrap();
        }
        // Not catalogs for this app: another domain, an alias directory, a stray file.
        std::fs::create_dir_all(tmp.path().join("es/LC_MESSAGES")).unwrap();
        std::fs::write(tmp.path().join("es/LC_MESSAGES/other.mo"), b"").unwrap();
        std::fs::create_dir_all(tmp.path().join("en.utf8/LC_MESSAGES")).unwrap();
        std::fs::write(tmp.path().join("en.utf8/LC_MESSAGES/audiobooklet.mo"), b"").unwrap();
        std::fs::write(tmp.path().join("README"), b"").unwrap();
        assert_eq!(
            available_languages_in(tmp.path()),
            ["en", "de", "fr", "pt_BR"],
            "English first, then the rest ordered by their own names (Deutsch, Français, Português (Brasil))"
        );
    }

    #[test]
    fn languages_are_named_in_themselves() {
        assert_eq!(language_name("en"), "English");
        assert_eq!(language_name("de"), "Deutsch");
        assert_eq!(language_name("xx"), "xx", "unknown codes are shown as the code");
    }

    #[test]
    fn untranslated_text_passes_through() {
        assert_eq!(tr("Settings"), "Settings");
        assert_eq!(tr_ctx("menu", "Settings"), "Settings");
    }

    #[test]
    fn named_placeholders_are_filled_in_any_order() {
        assert_eq!(
            tr_args("{b} follows {a}", &[("a", "one"), ("b", "two")]),
            "two follows one"
        );
    }

    #[test]
    fn unknown_placeholders_are_left_alone() {
        assert_eq!(tr_args("Hello {who}", &[("other", "x")]), "Hello {who}");
    }

    #[test]
    fn values_containing_braces_are_not_reinterpreted() {
        assert_eq!(
            tr_args("{a} and {b}", &[("a", "{b}"), ("b", "B")]),
            "{b} and B",
            "a substituted value is never expanded again"
        );
    }

    #[test]
    fn plurals_use_the_english_rule_without_a_catalog() {
        assert_eq!(ntr("{count} chapter", "{count} chapters", 1), "1 chapter");
        assert_eq!(ntr("{count} chapter", "{count} chapters", 0), "0 chapters");
        assert_eq!(ntr("{count} chapter", "{count} chapters", 2), "2 chapters");
        assert_eq!(
            ntr_args("{count} book in {series}", "{count} books in {series}", 3, &[("series", "Dune")]),
            "3 books in Dune"
        );
    }
}
