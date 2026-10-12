//! Opens files and links in the desktop's own apps (the latest log in a text editor, the website
//! in a browser) with the environment the user's session had — not the one the AppImage set up.
//!
//! The AppImage's AppRun hook (written by `scripts/build-appimage.sh`) points `LD_LIBRARY_PATH`,
//! `GTK_PATH`, `GIO_MODULE_DIR`, `GST_PLUGIN_SYSTEM_PATH` and others at the bundle, and a launched
//! app inherits them: the host's editor then loads our (older) GLib instead of its own and dies
//! before showing a window — "mousepad: symbol lookup error: .../libatspi.so.0: undefined symbol:
//! g_once_init_leave_pointer", so "Open latest log file" did nothing. The hook records the host's
//! values first (`AUDIOBOOKLET_BUNDLE_ENV` names the variables it changes, `AUDIOBOOKLET_HOST_<VAR>`
//! holds a value that was set); this module puts them back on the launch context.
//!
//! Only for launching *host* apps: the app re-executing itself (`crash_reporting`'s server,
//! `main.rs`'s respawn) needs the bundle's environment and keeps using `std::process` as before.
//! Outside an AppImage (Flatpak, `.deb`, `cargo run`) nothing was recorded, so nothing changes.

use std::ffi::OsString;

use gtk4::{gio, glib};
use gtk4::prelude::*;

/// Names the variables the AppRun hook changed (space-separated); absent outside the AppImage.
const BUNDLE_ENV: &str = "AUDIOBOOKLET_BUNDLE_ENV";
/// Prefix of the variables holding the host's values, e.g. `AUDIOBOOKLET_HOST_LD_LIBRARY_PATH`.
const HOST_PREFIX: &str = "AUDIOBOOKLET_HOST_";
/// Set by the AppImage runtime (and `APPDIR` by the hook too); they describe this AppImage, not
/// the launched app, which would only be confused by them — another AppImage's AppRun among them.
const APPIMAGE_RUNTIME_VARS: [&str; 4] = ["APPDIR", "APPIMAGE", "ARGV0", "OWD"];

/// Opens `uri` with the desktop's default handler for it, the way
/// `gio::AppInfo::launch_default_for_uri` does, but with the host's environment.
pub(crate) fn launch_default_for_uri(uri: &str) -> Result<(), glib::Error> {
    let context = gio::AppLaunchContext::new();
    for (name, value) in host_env_changes(|name| std::env::var_os(name)) {
        match value {
            Some(value) => context.setenv(&name, value),
            None => context.unsetenv(&name),
        }
    }
    gio::AppInfo::launch_default_for_uri(uri, Some(&context))
}

/// What a launched app's environment needs changed: `(name, Some(value))` to set, `(name, None)`
/// to remove. `get` reads this process's environment (a parameter so tests can fake it).
fn host_env_changes(get: impl Fn(&str) -> Option<OsString>) -> Vec<(String, Option<OsString>)> {
    let Some(names) = get(BUNDLE_ENV) else { return Vec::new() };
    let mut changes = Vec::new();
    for name in names.to_string_lossy().split_whitespace() {
        let host_var = format!("{HOST_PREFIX}{name}");
        changes.push((name.to_owned(), get(&host_var)));
        changes.push((host_var, None));
    }
    changes.push((BUNDLE_ENV.to_owned(), None));
    for name in APPIMAGE_RUNTIME_VARS {
        changes.push((name.to_owned(), None));
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn changes(env: &[(&str, &str)]) -> HashMap<String, Option<String>> {
        let env: HashMap<&str, &str> = env.iter().copied().collect();
        host_env_changes(|name| env.get(name).map(OsString::from))
            .into_iter()
            .map(|(name, value)| (name, value.map(|v| v.into_string().unwrap())))
            .collect()
    }

    #[test]
    fn outside_an_appimage_nothing_changes() {
        assert!(changes(&[("LD_LIBRARY_PATH", "/opt/lib"), ("XDG_DATA_DIRS", "/usr/share")]).is_empty());
    }

    #[test]
    fn variables_the_host_had_are_restored() {
        let c = changes(&[
            (BUNDLE_ENV, "LD_LIBRARY_PATH XDG_DATA_DIRS"),
            ("LD_LIBRARY_PATH", "/tmp/.mount_x/usr/lib:/opt/lib"),
            ("AUDIOBOOKLET_HOST_LD_LIBRARY_PATH", "/opt/lib"),
            ("XDG_DATA_DIRS", "/tmp/.mount_x/usr/share:/usr/share:/usr/local/share"),
            ("AUDIOBOOKLET_HOST_XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
        ]);
        assert_eq!(c["LD_LIBRARY_PATH"].as_deref(), Some("/opt/lib"));
        assert_eq!(c["XDG_DATA_DIRS"].as_deref(), Some("/usr/local/share:/usr/share"));
    }

    #[test]
    fn variables_the_host_did_not_have_are_removed() {
        let c = changes(&[
            (BUNDLE_ENV, "LD_LIBRARY_PATH GST_PLUGIN_SYSTEM_PATH"),
            ("LD_LIBRARY_PATH", "/tmp/.mount_x/usr/lib"),
            ("GST_PLUGIN_SYSTEM_PATH", "/tmp/.mount_x/usr/lib/gstreamer-1.0"),
        ]);
        assert_eq!(c["LD_LIBRARY_PATH"], None);
        assert_eq!(c["GST_PLUGIN_SYSTEM_PATH"], None);
    }

    #[test]
    fn the_bookkeeping_and_appimage_variables_are_removed() {
        let c = changes(&[(BUNDLE_ENV, "GTK_PATH"), ("AUDIOBOOKLET_HOST_GTK_PATH", "/usr/lib/gtk")]);
        assert_eq!(c["GTK_PATH"].as_deref(), Some("/usr/lib/gtk"));
        for name in [BUNDLE_ENV, "AUDIOBOOKLET_HOST_GTK_PATH", "APPDIR", "APPIMAGE", "ARGV0", "OWD"] {
            assert_eq!(c[name], None, "{name} should be removed");
        }
    }
}
