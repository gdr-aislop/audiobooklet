#!/usr/bin/env bash
# Tests scripts/appimage-gst-env.sh (the AppImage launcher's GStreamer setup) against real
# GStreamer plugins, and that the launcher hook written by build-appimage.sh uses it.
#
#   scripts/test-appimage-gst-env.sh
#
# Needs gst-inspect-1.0 (gstreamer1.0-tools) and the GStreamer plugins installed; without them it
# says so and exits 0 so a machine that can't run it isn't failed for that.
set -u

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$REPO_ROOT/scripts/appimage-gst-env.sh"
BUILD="$REPO_ROOT/scripts/build-appimage.sh"

if ! command -v gst-inspect-1.0 >/dev/null 2>&1; then
    echo "SKIP: gst-inspect-1.0 not found (install gstreamer1.0-tools)"
    exit 0
fi
PLUGINS_SRC="$(pkg-config --variable=pluginsdir gstreamer-1.0 2>/dev/null || true)"
if [ -z "$PLUGINS_SRC" ] || ! ls "$PLUGINS_SRC"/libgstcoreelements.so >/dev/null 2>&1; then
    PLUGINS_SRC="$(dirname "$(ls /usr/lib/*/gstreamer-1.0/libgstcoreelements.so 2>/dev/null | head -n 1)")"
fi
if [ ! -f "$PLUGINS_SRC/libgstcoreelements.so" ]; then
    echo "SKIP: no GStreamer plugins found"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAILED=0
check() { # check "what" <command...>
    local what="$1"
    shift
    if "$@"; then echo "ok    $what"; else echo "FAIL  $what"; FAILED=1; fi
}

# A fake AppDir ("a mount") holding a few real plugins at a given path, files' times preserved
# (a mount of the same AppImage shows the same files with the same times).
make_mount() {
    mkdir -p "$1/usr/lib/gstreamer-1.0"
    for plugin in coreelements typefindfunctions playback audioconvert; do
        [ -f "$PLUGINS_SRC/libgst$plugin.so" ] && cp -a "$PLUGINS_SRC/libgst$plugin.so" "$1/usr/lib/gstreamer-1.0/"
    done
}

export HOME="$TMP/home"
mkdir -p "$HOME"
unset XDG_CACHE_HOME GST_PLUGIN_PATH GST_REGISTRY GST_PLUGIN_SYSTEM_PATH GST_REGISTRY_FORK GST_PLUGIN_SCANNER
CACHE="$HOME/.cache/audiobooklet"
LINK="$CACHE/gstreamer-plugins"
REGISTRY="$CACHE/gstreamer-registry.bin"

# Runs GStreamer the way the app does (in-process scan), in the environment the setup produced for
# the given mount; prints nothing.
run_in_mount() {
    ( . "$SETUP"; audiobooklet_gst_setup "$1"; GST_REGISTRY_FORK=no gst-inspect-1.0 coreelements >/dev/null 2>&1 )
}
mtime() { stat -c %y "$1" 2>/dev/null; }
points_to() { [ "$(readlink -f "$LINK")" = "$(readlink -f "$1/usr/lib/gstreamer-1.0")" ]; }

# --- 1. the cache survives the AppImage mounting at a new path --------------------------------
make_mount "$TMP/mnt1"
run_in_mount "$TMP/mnt1"
check "the first start writes the plugin cache" test -s "$REGISTRY"
check "the link points at this mount's plugins" points_to "$TMP/mnt1"
first="$(mtime "$REGISTRY")"
rm -rf "$TMP/mnt1" # the previous run's mount is gone
make_mount "$TMP/mnt2"
run_in_mount "$TMP/mnt2"
check "the link follows to the new mount" points_to "$TMP/mnt2"
check "a start from a new mount path does not rescan: the cache is untouched" test "$(mtime "$REGISTRY")" = "$first"

# The problem this fixes, for the record: the same move without the link rescans.
make_mount "$TMP/old"
( export GST_REGISTRY="$TMP/plain-registry.bin" GST_PLUGIN_SYSTEM_PATH="$TMP/old/usr/lib/gstreamer-1.0"; GST_REGISTRY_FORK=no gst-inspect-1.0 coreelements >/dev/null 2>&1 )
plain="$(mtime "$TMP/plain-registry.bin")"
make_mount "$TMP/new"
( export GST_REGISTRY="$TMP/plain-registry.bin" GST_PLUGIN_SYSTEM_PATH="$TMP/new/usr/lib/gstreamer-1.0"; GST_REGISTRY_FORK=no gst-inspect-1.0 coreelements >/dev/null 2>&1 )
check "without the link, a new mount path does rescan (the cache is rewritten)" test "$(mtime "$TMP/plain-registry.bin")" != "$plain"

# --- 2. a changed plugin is still noticed -------------------------------------------------------
before="$(mtime "$REGISTRY")"
sleep 1
touch "$TMP/mnt2/usr/lib/gstreamer-1.0/libgstcoreelements.so" # what a new AppImage version's plugin looks like
run_in_mount "$TMP/mnt2"
check "a plugin that changed is scanned again" test "$(mtime "$REGISTRY")" != "$before"

# --- 3. two launches at once --------------------------------------------------------------------
make_mount "$TMP/mnt3"
run_in_mount "$TMP/mnt3" # mnt2 is still mounted: another launch is running
check "while another launch's mount is alive its link is left alone" points_to "$TMP/mnt2"
env_path="$( . "$SETUP"; audiobooklet_gst_setup "$TMP/mnt3"; printf '%s' "$GST_PLUGIN_SYSTEM_PATH" )"
check "and this launch uses the link all the same" test "$env_path" = "$LINK"
rm -rf "$TMP/mnt2"
run_in_mount "$TMP/mnt3"
check "once that mount is gone the link moves to this one" points_to "$TMP/mnt3"

# --- 4. no usable cache folder: as before ---------------------------------------------------------
fallback="$( HOME=/proc/no-such-home; unset XDG_CACHE_HOME; . "$SETUP"; audiobooklet_gst_setup "$TMP/mnt3"; printf '%s|%s' "$GST_PLUGIN_SYSTEM_PATH" "${GST_REGISTRY:-unset}" )"
check "with no usable cache folder the plugins are used where they are, with no cache of ours" test "$fallback" = "$TMP/mnt3/usr/lib/gstreamer-1.0|unset"
nohome="$( unset HOME XDG_CACHE_HOME; . "$SETUP"; audiobooklet_gst_setup "$TMP/mnt3"; printf '%s|%s' "$GST_PLUGIN_SYSTEM_PATH" "${GST_REGISTRY:-unset}" )"
check "with no home folder at all, the same" test "$nohome" = "$TMP/mnt3/usr/lib/gstreamer-1.0|unset"

# --- 5. the launcher hook that build-appimage.sh writes -------------------------------------------
hook="$TMP/hook.sh"
sed -n "/apprun-hooks\/\$APP_NAME.sh\" <<'EOF'/,/^EOF\$/p" "$BUILD" | sed '1d;$d' > "$hook"
check "the launcher hook was found in build-appimage.sh" test -s "$hook"
mkdir -p "$TMP/mnt4/usr/share/audiobooklet"
make_mount "$TMP/mnt4"
cp "$SETUP" "$TMP/mnt4/usr/share/audiobooklet/gst-env.sh"
rm -f "$LINK"
hook_env="$( HOME="$TMP/home" GST_REGISTRY=/host/registry.bin; export GST_REGISTRY; APPDIR="$TMP/mnt4"; . "$hook"; printf '%s\n%s\n%s\n%s\n%s' "$GST_PLUGIN_SYSTEM_PATH" "$GST_REGISTRY" "$GST_REGISTRY_FORK" "$AUDIOBOOKLET_HOST_GST_REGISTRY" "$AUDIOBOOKLET_BUNDLE_ENV" )"
check "the hook uses the fixed plugin link" test "$(sed -n 1p <<<"$hook_env")" = "$LINK"
check "the hook uses the app's own cache file" test "$(sed -n 2p <<<"$hook_env")" = "$REGISTRY"
check "the hook still scans in-process" test "$(sed -n 3p <<<"$hook_env")" = "no"
check "the hook records the host's cache setting for the apps it launches" test "$(sed -n 4p <<<"$hook_env")" = "/host/registry.bin"
check "and lists the cache setting among what those apps must not inherit" grep -qw GST_REGISTRY <<<"$(sed -n 5p <<<"$hook_env")"

if [ "$FAILED" -ne 0 ]; then
    echo "FAILED"
    exit 1
fi
echo "all passed"
