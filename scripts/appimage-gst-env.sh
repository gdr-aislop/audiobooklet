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
#
# On top of that the AppImage ships a cache built at release time (appimage-gst-registry.sh), used
# where the plugin link can sit at the path it was built for: see audiobooklet__gst_prewarmed.

# Points <link> (a symlink) at <plugins> so the plugin path stays the same from one launch to the
# next. Another launch may be running from a different mount (a second launch only asks the first
# to show its window, and exits at once): its plugins are loaded on demand, so the link must stay
# pointing at a mount that outlives it. Only a link that goes nowhere, or already points here, is
# replaced. Returns non-zero when the link can't be made.
audiobooklet__gst_keep_link() { # <plugins> <link>
    local plugins="$1" link="$2"
    local current=""
    if [ -L "$link" ] && [ -d "$link/" ]; then
        current="$(readlink -f "$link" 2>/dev/null)"
    fi
    local mine
    mine="$(readlink -f "$plugins" 2>/dev/null || printf '%s' "$plugins")"
    if [ -z "$current" ] || [ "$current" = "$mine" ]; then
        ln -sfn "$plugins" "$link" 2>/dev/null || return 1
    fi
    return 0
}

# The cache built when the AppImage was (scripts/appimage-gst-registry.sh), used when this
# system's runtime folder is where it was built for: the plugins are reached through a link there,
# and a copy of the cache goes into the user's cache folder the first time, and whenever a new
# AppImage brings a new one. GStreamer then finds every plugin as recorded and scans nothing.
# Returns non-zero when it doesn't apply (the caller then does the plain per-user setup).
audiobooklet__gst_prewarmed() { # <appdir> <cache folder>
    local appdir="$1" cache="$2"
    local plugins="$appdir/usr/lib/gstreamer-1.0"
    local share="$appdir/usr/share/audiobooklet"
    [ -f "$share/gst-registry.bin" ] && [ -r "$share/gst-registry.link" ] && [ -r "$share/gst-registry.id" ] || return 1
    local runtime="${XDG_RUNTIME_DIR:-}"
    [ -n "$runtime" ] && [ -d "$runtime" ] && [ -w "$runtime" ] || return 1
    local link="${runtime%/}/audiobooklet-gstreamer-plugins"
    [ "$(cat "$share/gst-registry.link")" = "$link" ] || return 1
    audiobooklet__gst_keep_link "$plugins" "$link" || return 1

    local registry="$cache/gstreamer-registry-prewarmed.bin"
    local shipped_id
    shipped_id="$(cat "$share/gst-registry.id")"
    if [ ! -s "$registry" ] || [ "$(cat "$registry.id" 2>/dev/null)" != "$shipped_id" ]; then
        local copy="$registry.tmp.$$"
        if cp "$share/gst-registry.bin" "$copy" 2>/dev/null && mv -f "$copy" "$registry" 2>/dev/null; then
            printf '%s' "$shipped_id" > "$registry.id" 2>/dev/null
        else
            rm -f "$copy"
        fi
    fi
    # The cache file of the per-user setup below (an earlier version's): nothing uses it now.
    rm -f "$cache/gstreamer-registry.bin" 2>/dev/null

    export GST_PLUGIN_SYSTEM_PATH="$link"
    export GST_REGISTRY="$registry"
}

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

    audiobooklet__gst_prewarmed "$appdir" "$cache" && return 0

    if ! audiobooklet__gst_keep_link "$plugins" "$link"; then
        export GST_PLUGIN_SYSTEM_PATH="$plugins"
        return 0
    fi

    export GST_PLUGIN_SYSTEM_PATH="$link"
    export GST_REGISTRY="$cache/gstreamer-registry.bin"
}
