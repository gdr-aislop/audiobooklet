#!/usr/bin/env bash
# Sourced by scripts/build-appimage.sh (and tested by scripts/test-appimage-gst-env.sh): builds the
# GStreamer plugin cache that ships inside the AppImage, so the first start on a phone doesn't
# scan every bundled plugin (seconds, in-process).
#
# GStreamer's cache records each plugin by full path, size and modification time and trusts it
# only while all three match. So the cache is built with the plugins at the path they will have
# on the phone — the fixed symlink the launcher makes (appimage-gst-env.sh) in $XDG_RUNTIME_DIR —
# and with modification times cut to whole seconds, which is all the AppImage's squashfs keeps.
#
# Files written into the AppDir, under usr/share/audiobooklet/:
#   gst-registry.bin   the cache
#   gst-registry.link  the plugin-folder link path it was built for
#   gst-registry.id    a checksum of the cache, to tell one build's from another's

AUDIOBOOKLET_GST_LINK_NAME="audiobooklet-gstreamer-plugins"

# Runs gst-inspect-1.0 the way the launcher sets GStreamer up, against <registry> and the plugin
# folder reached through <link>. Extra arguments go to gst-inspect-1.0. Quiet.
audiobooklet__gst_inspect() { # <appdir> <link> <registry> [gst-inspect args...]
    local appdir="$1" link="$2" registry="$3"
    shift 3
    local scanner=()
    if [ -x "$appdir/usr/libexec/gstreamer-1.0/gst-plugin-scanner" ]; then
        scanner=(GST_PLUGIN_SCANNER="$appdir/usr/libexec/gstreamer-1.0/gst-plugin-scanner")
    fi
    env -u GST_PLUGIN_PATH -u GST_PLUGIN_PATH_1_0 -u GST_PLUGIN_SYSTEM_PATH_1_0 \
        LD_LIBRARY_PATH="$appdir/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        GST_PLUGIN_SYSTEM_PATH="$link" GST_REGISTRY="$registry" GST_REGISTRY_FORK=no \
        "${scanner[@]}" gst-inspect-1.0 "$@"
}

# Builds the cache for <appdir>'s plugins, reached at <rundir>/audiobooklet-gstreamer-plugins.
# Call it once the plugin files are final (nothing may touch them afterwards). Returns non-zero,
# having said why, when it can't produce a trustworthy cache.
audiobooklet_gst_registry_build() { # <appdir> <rundir>
    local appdir="$1" rundir="$2"
    local plugins="$appdir/usr/lib/gstreamer-1.0"
    local share="$appdir/usr/share/audiobooklet"
    local link="$rundir/$AUDIOBOOKLET_GST_LINK_NAME"
    local registry
    registry="$(mktemp -u "${TMPDIR:-/tmp}/gst-registry.XXXXXX")"

    mkdir -p "$rundir" || { echo "gst-registry: cannot create $rundir" >&2; return 1; }
    ln -sfn "$plugins" "$link" || { echo "gst-registry: cannot link $link" >&2; return 1; }

    # Whole seconds only: what the AppImage's squashfs (and so every phone) shows.
    local file seconds
    for file in "$plugins"/*.so; do
        [ -f "$file" ] || continue
        seconds="$(stat -c %Y "$file")"
        touch -d "@$seconds" "$file"
    done

    audiobooklet__gst_inspect "$appdir" "$link" "$registry" -a >/dev/null 2>&1
    if [ ! -s "$registry" ]; then
        echo "gst-registry: GStreamer wrote no cache" >&2
        return 1
    fi
    # A plugin that failed to load here would be recorded as broken, and stay broken on the phone.
    if ! audiobooklet__gst_inspect "$appdir" "$link" "$registry" -b 2>&1 | grep -q 'Total count: 0 blacklisted'; then
        echo "gst-registry: some plugins failed to load while building the cache:" >&2
        audiobooklet__gst_inspect "$appdir" "$link" "$registry" -b >&2 || true
        rm -f "$registry"
        return 1
    fi

    install -Dm644 "$registry" "$share/gst-registry.bin"
    printf '%s' "$link" > "$share/gst-registry.link"
    sha256sum "$registry" | cut -d' ' -f1 | tr -d '\n' > "$share/gst-registry.id"
    rm -f "$registry"
}

# That what the cache is for is really in it: the elements playback needs. For the release build.
audiobooklet_gst_registry_verify() { # <appdir>
    local appdir="$1"
    local link registry element
    link="$(cat "$appdir/usr/share/audiobooklet/gst-registry.link")"
    registry="$appdir/usr/share/audiobooklet/gst-registry.bin"
    local copy
    copy="$(mktemp "${TMPDIR:-/tmp}/gst-registry-verify.XXXXXX")"
    cp "$registry" "$copy"
    for element in playbin souphttpsrc autoaudiosink audioconvert audioresample pulsesink avdec_aac; do
        if ! audiobooklet__gst_inspect "$appdir" "$link" "$copy" --exists "$element" >/dev/null 2>&1; then
            echo "gst-registry: element $element is missing from the cache" >&2
            rm -f "$copy"
            return 1
        fi
    done
    rm -f "$copy"
}

# Whether the shipped cache of <appdir> (a mounted or extracted AppDir, so the files have the times
# the phone will see) is accepted as it is: GStreamer, run against a copy of it with the plugins
# reached through the link, must not need to rewrite it.
audiobooklet_gst_registry_matches() { # <appdir> <rundir>
    local appdir="$1" rundir="$2"
    local link="$rundir/$AUDIOBOOKLET_GST_LINK_NAME"
    local copy before after
    mkdir -p "$rundir" && ln -sfn "$appdir/usr/lib/gstreamer-1.0" "$link" || return 1
    copy="$(mktemp "${TMPDIR:-/tmp}/gst-registry-match.XXXXXX")"
    cp "$appdir/usr/share/audiobooklet/gst-registry.bin" "$copy"
    before="$(sha256sum "$copy" | cut -d' ' -f1)"
    audiobooklet__gst_inspect "$appdir" "$link" "$copy" -a >/dev/null 2>&1
    after="$(sha256sum "$copy" | cut -d' ' -f1)"
    rm -f "$copy"
    [ "$before" = "$after" ]
}
