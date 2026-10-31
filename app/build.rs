//! Captures the short git commit hash at compile time as the `ABS_APP_GIT_HASH` env var, read by
//! `application::version_line()` and main's startup diagnostics log line — so a built binary
//! (and a user's pasted log) identifies exactly which commit it came from, without anyone having
//! to ask. Falls back to `"unknown"` when this isn't a git checkout (a distro source tarball
//! build) or `git` isn't on `PATH`, rather than failing the build over a cosmetic detail.
//!
//! When it re-runs: on a new commit or a branch switch (git's `HEAD`, the branch's ref file and
//! `packed-refs`, whichever exist) and when `data/` changes (the resources below). Emitting no
//! `rerun-if-changed` at all, as this script once did, does *not* mean "every build": Cargo then
//! re-runs it only when a file of this crate changes, so a commit that touched only another
//! crate (`abs-core`, say) kept the previous commit's hash, and a user's log named the wrong build.
//! Only paths that exist are named: a missing one would make Cargo re-run this script, and so
//! recompile the whole app, on every build. Without git (a source tarball) nothing is named and
//! Cargo's own rule applies.
//!
//! It also compiles `data/resources.gresource.xml` (the icons the system theme can't be relied on
//! for, see `crate::icons`) into `$OUT_DIR/audiobooklet.gresource` with `glib-compile-resources`. Unlike
//! the hash, that one is not optional: without it the build fails, rather than shipping a binary
//! that shows "missing icon" placeholders. The tool comes with GLib's development files
//! (`libglib2.0-dev-bin`, pulled in by `libgtk-4-dev`; the GNOME SDK has it too).

use std::process::Command;

fn main() {
    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ABS_APP_GIT_HASH={hash}");
    watch_git_head();

    println!("cargo:rerun-if-changed=data");
    compile_resources();
}

/// `git <args>`'s trimmed output, or `None` if git isn't there, fails, or prints nothing.
fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Asks Cargo to re-run this script when the commit changes: `HEAD` (a branch switch, a detached
/// checkout, a commit while detached), the checked-out branch's ref (a commit on it) and
/// `packed-refs` (where git may keep that ref instead). Worktrees keep `HEAD` in their own git
/// directory and refs in the shared one, hence the two directories.
fn watch_git_head() {
    let current = std::env::current_dir().unwrap_or_default();
    let absolute = |path: String| {
        let path = std::path::PathBuf::from(path);
        if path.is_absolute() { path } else { current.join(path) }
    };
    let Some(git_dir) = git(&["rev-parse", "--git-dir"]).map(absolute) else { return };
    let common_dir = git(&["rev-parse", "--git-common-dir"]).map(absolute).unwrap_or_else(|| git_dir.clone());
    let mut watched = vec![git_dir.join("HEAD"), common_dir.join("packed-refs")];
    if let Some(branch_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(common_dir.join(branch_ref));
    }
    for path in watched.into_iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn compile_resources() {
    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR for build scripts");
    let status = Command::new("glib-compile-resources")
        .args(["--sourcedir", "data", "--target"])
        .arg(std::path::Path::new(&out_dir).join("audiobooklet.gresource"))
        .arg("data/resources.gresource.xml")
        .status()
        .unwrap_or_else(|err| panic!("could not run glib-compile-resources (install libglib2.0-dev-bin): {err}"));
    assert!(status.success(), "glib-compile-resources failed on data/resources.gresource.xml ({status})");
}
