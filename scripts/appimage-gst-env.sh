#!/usr/bin/env bash
# Sourced by the AppImage's launcher hook (apprun-hooks/audiobooklet.sh, written by
# build-appimage.sh), which calls `audiobooklet_gst_setup "$APPDIR"`. Kept in a file of its own so
# scripts/test-appimage-gst-env.sh can run it against real GStreamer plugins.
#
# Why: an AppImage mounts itself at a new /tmp/.mount_XXXXXX on every launch, and GStreamer's
# plugin cache records each plugin by its full path, so with the bundled plugins at a new path
# every start looked like "all plugins are new" and scanned them again (in-process, because the
# hook sets GST_REGISTRY_FORK=no), seconds on a phone. Pointing GStreamer at a fixed path that is
# a symlink to this launch's plugin folder, with a cache file of our own, lets the cache survive:
# unchanged plugins are recognised by path, size and modification time, and a changed one (a new
# AppImage version) is scanned again. The cache file is ours alone, so the host's GStreamer and
# this one don't overwrite each other's.

audiobooklet_gst_setup() {
    local appdir="$1"
    local plugins="$appdir/usr/lib/gstreamer-1.0"
    local cache="${XDG_CACHE_HOME:-${HOME:-}/.cache}/audiobooklet"
    local link="$cache/gstreamer-plugins"

    # Without a usable cache folder, behave as before: the plugins where they are, scanned each start.
    if [ -z "${HOME:-}${XDG_CACHE_HOME:-}" ] || ! mkdir -p "$cache" 2>/dev/null; then
        export GST_PLUGIN_SYSTEM_PATH="$plugins"
        return 0
    fi

    # Another launch may be running from a different mount (a second launch only asks the first to
    # show its window, and exits at once): its plugins are loaded on demand, so the link must stay
    # pointing at a mount that outlives it. Only a link that goes nowhere, or already points here,
    # is replaced.
    local current=""
    if [ -L "$link" ] && [ -d "$link/" ]; then
        current="$(readlink -f "$link" 2>/dev/null)"
    fi
    local mine
    mine="$(readlink -f "$plugins" 2>/dev/null || printf '%s' "$plugins")"
    if [ -z "$current" ] || [ "$current" = "$mine" ]; then
        if ! ln -sfn "$plugins" "$link" 2>/dev/null; then
            export GST_PLUGIN_SYSTEM_PATH="$plugins"
            return 0
        fi
    fi

    export GST_PLUGIN_SYSTEM_PATH="$link"
    export GST_REGISTRY="$cache/gstreamer-registry.bin"
}
