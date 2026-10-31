#!/usr/bin/env bash
#
# Build an AppImage of audiobooklet for x86_64 or aarch64.
#
# Works both locally (any glibc Linux with the GTK4/libadwaita/GStreamer
# packages installed) and inside CI containers (.github/workflows/release.yml,
# debian:bookworm). The resulting AppImage bundles GTK4, libadwaita, the
# GStreamer plugins and the Adwaita icon theme, so it runs on systems that
# have none of those installed.
#
# Usage:
#   scripts/build-appimage.sh [--arch x86_64|aarch64] [--version V] [--output-dir DIR]
#
# Environment:
#   ABS_APP_BIN         prebuilt release binary to package (skips `cargo build`)
#   LINUXDEPLOY         linuxdeploy executable to use (skips the download)
#   APPIMAGE_TOOLS_DIR  cache dir for the linuxdeploy download
#                       (default: build/appimage-tools)
#
# Bundling strategy:
#   - linuxdeploy resolves and copies every DT_NEEDED library (GTK4, adw,
#     glib, sqlite, openssl, ...) into AppDir/usr/lib and patches rpaths.
#   - Resources that are dlopen()ed and therefore invisible to ldd are copied
#     by this script and handed to linuxdeploy via --deploy-deps-only so their
#     own dependencies get pulled in too: GStreamer plugins, gio modules
#     (TLS, dconf), gdk-pixbuf loaders, GTK4 modules.
#   - The generated apprun-hooks/audiobooklet.sh is sourced by linuxdeploy's
#     default AppRun and points the runtime at all of the above.
#
# Update information: LDAI_UPDATE_INFORMATION (read by linuxdeploy's bundled
# appimage output plugin, not a linuxdeploy CLI flag) is set to a
# gh-releases-zsync string below, so the AppImage embeds enough to be
# update-checked by AppImageUpdate/Gear Lever against this repo's GitHub
# releases, and the plugin generates the matching .zsync file alongside the
# .AppImage. Requires `zsyncmake` (the `zsync` package) on PATH.

set -euo pipefail

APP_NAME="audiobooklet"
APP_ID="io.github.gdr_aislop.audiobooklet"
GH_OWNER="gdr-aislop"
GH_REPO="audiobooklet"
LINUXDEPLOY_VERSION="1-alpha-20251107-1" # pinned for reproducible builds

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

die() { echo "error: $*" >&2; exit 1; }
warn() { echo "warning: $*" >&2; }

usage() {
    cat <<'USAGE'
Usage: scripts/build-appimage.sh [--arch x86_64|aarch64] [--version V] [--output-dir DIR]

Builds an AppImage of audiobooklet, bundling GTK4/libadwaita, GStreamer plugins,
glib schemas, pixbuf loaders and the Adwaita icon theme.

Environment:
  ABS_APP_BIN         prebuilt release binary to package (skips `cargo build`)
  LINUXDEPLOY         linuxdeploy executable to use (skips the download)
  APPIMAGE_TOOLS_DIR  cache dir for the linuxdeploy download (default: build/appimage-tools)
USAGE
    exit 0
}

ARCH=""
VERSION=""
OUTPUT_DIR="$REPO_ROOT/build/appimage"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --arch) ARCH="${2:-}"; shift 2 ;;
        --version) VERSION="${2:-}"; shift 2 ;;
        --output-dir) OUTPUT_DIR="${2:-}"; shift 2 ;;
        --help|-h) usage ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

# --- normalize architecture (linuxdeploy asset naming: x86_64/aarch64) ---
if [ -z "$ARCH" ]; then
    case "$(uname -m)" in
        x86_64|amd64) ARCH="x86_64" ;;
        aarch64|arm64) ARCH="aarch64" ;;
        *) die "unsupported architecture: $(uname -m) (use --arch)" ;;
    esac
fi
case "$ARCH" in
    x86_64) LINUXDEPLOY_ASSET="linuxdeploy-x86_64.AppImage" ;;
    aarch64) LINUXDEPLOY_ASSET="linuxdeploy-aarch64.AppImage" ;;
    *) die "unsupported --arch: $ARCH (use x86_64 or aarch64)" ;;
esac

# --- determine version: flag > git describe > Cargo.toml workspace version ---
if [ -z "$VERSION" ]; then
    VERSION="$(git describe --tags --always --dirty 2>/dev/null || true)"
fi
if [ -z "$VERSION" ]; then
    VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
fi
[ -n "$VERSION" ] || die "cannot determine version (pass --version)"
VERSION="${VERSION//\//-}" # keep it filename-safe (branch names can contain /)

# --- tool and source availability checks ---
# zsyncmake generates the .zsync file alongside the AppImage (see the update-information note
# above) — the appimage output plugin shells out to it once LDAI_UPDATE_INFORMATION is set.
for tool in cargo pkg-config glib-compile-schemas curl zsyncmake rsvg-convert; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"
done

# gdk-pixbuf-query-loaders is not on $PATH on Debian/Ubuntu (it ships in
# libgdk-pixbuf-2.0-0 under the multiarch libdir); resolve it explicitly.
if command -v gdk-pixbuf-query-loaders >/dev/null 2>&1; then
    PIXBUF_QUERY="$(command -v gdk-pixbuf-query-loaders)"
else
    PIXBUF_QUERY="$(pkg-config --variable=libdir gdk-pixbuf-2.0 2>/dev/null || true)/gdk-pixbuf-2.0/gdk-pixbuf-query-loaders"
fi
[ -x "$PIXBUF_QUERY" ] || die "gdk-pixbuf-query-loaders not found (is libgdk-pixbuf-2.0-0 installed?)"

# Resolve system paths via pkg-config (works on Debian/Ubuntu layouts and others).
pkg_var() { # <variable> <package> <fallback>
    local value
    value="$(pkg-config --variable="$1" "$2" 2>/dev/null || true)"
    echo "${value:-$3}"
}

SCHEMAS_SRC="$(pkg_var schemasdir gio-2.0 /usr/share/glib-2.0/schemas)"
GIO_MODULES_SRC="$(pkg_var giomoduledir gio-2.0 '')"
PIXBUF_MODULES_SRC="$(pkg_var gdk_pixbuf_moduledir gdk-pixbuf-2.0 '')"
GTK4_MODULES_SRC="$(pkg_var libdir gtk4 '')/gtk-4.0"
# Debian names the variable "pluginsdir" (with scanner dir "pluginscannerdir");
# other distros use "plugindir".
GST_PLUGINS_SRC="$(pkg_var pluginsdir gstreamer-1.0 \
    "$(pkg_var plugindir gstreamer-1.0 "$(pkg_var libdir gstreamer-1.0 '')/gstreamer-1.0")")"

[ -d "$SCHEMAS_SRC" ] || die "glib schemas dir not found ($SCHEMAS_SRC); is libglib2.0-dev installed?"
[ -d "$GIO_MODULES_SRC" ] || die "gio modules dir not found; check gio-2.0.pc / libglib2.0-dev"
[ -d "$PIXBUF_MODULES_SRC" ] || die "gdk-pixbuf loaders dir not found; check gdk-pixbuf-2.0.pc / libgdk-pixbuf-2.0-dev"
[ -d "$GST_PLUGINS_SRC" ] || die "gstreamer plugins dir not found; check gstreamer-1.0.pc / libgstreamer1.0-dev"
ls "$GST_PLUGINS_SRC"/*.so >/dev/null 2>&1 || die "no GStreamer plugins found in $GST_PLUGINS_SRC (install gstreamer1.0-plugins-base/good, gstreamer1.0-libav)"

# GStreamer helper binary (scans plugin metadata); optional but recommended.
GST_SCANNER_SRC=""
for candidate in \
    "$(pkg_var pluginscannerdir gstreamer-1.0 '')/gst-plugin-scanner" \
    "$(pkg_var libexecdir gstreamer-1.0 '')/gstreamer-1.0/gst-plugin-scanner" \
    "$GST_PLUGINS_SRC/../gstreamer1.0/gstreamer-1.0/gst-plugin-scanner" \
    /usr/lib/*/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner; do
    if [ -x "$candidate" ]; then GST_SCANNER_SRC="$candidate"; break; fi
done

# --- build the app (unless a prebuilt binary was supplied) ---
BIN="${ABS_APP_BIN:-${CARGO_TARGET_DIR:-target}/release/audiobooklet}"
if [ -z "${ABS_APP_BIN:-}" ]; then
    cargo build --release -p audiobooklet
fi
[ -x "$BIN" ] || die "binary not found at $BIN (set ABS_APP_BIN to override)"

# --- fetch linuxdeploy (x86_64 and aarch64 assets are published upstream) ---
TOOLS_DIR="${APPIMAGE_TOOLS_DIR:-$REPO_ROOT/build/appimage-tools}"
LINUXDEPLOY_BIN="${LINUXDEPLOY:-$TOOLS_DIR/$LINUXDEPLOY_ASSET}"
if [ ! -x "$LINUXDEPLOY_BIN" ]; then
    mkdir -p "$TOOLS_DIR"
    curl -fsSL --retry 3 -o "$LINUXDEPLOY_BIN" \
        "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_VERSION/$LINUXDEPLOY_ASSET"
    chmod +x "$LINUXDEPLOY_BIN"
fi

# --- assemble the AppDir ---
APPDIR="$OUTPUT_DIR/$APP_NAME.AppDir"
rm -rf "$APPDIR"
mkdir -p "$OUTPUT_DIR"
# Clear previous outputs so the post-build rename can't pick up stale files.
rm -f "$OUTPUT_DIR"/*.AppImage "$OUTPUT_DIR"/*.zsync

install -Dm755 "$BIN" "$APPDIR/usr/bin/$APP_NAME"
if command -v strip >/dev/null 2>&1; then
    strip "$APPDIR/usr/bin/$APP_NAME"
fi
# Translations: the compiled .mo catalogs, plus the .desktop and metainfo with their translated
# fields merged in (plain copies while po/LINGUAS is empty). The app finds the catalogs at
# <exe dir>/../share/locale, i.e. $APPDIR/usr/share/locale (app/src/i18n.rs).
L10N_DIR="$OUTPUT_DIR/l10n"
./scripts/build-l10n.sh "$L10N_DIR"
install -Dm644 "$L10N_DIR/$APP_ID.desktop" "$APPDIR/usr/share/applications/$APP_ID.desktop"
install -Dm644 "app/assets/$APP_ID.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"
# A PNG rendering too: the AppImage's .DirIcon (its thumbnail in file managers and AppImage
# catalogs) should be a PNG, which not every thumbnailer can make from an SVG.
ICON_PNG="$APPDIR/usr/share/icons/hicolor/256x256/apps/$APP_ID.png"
mkdir -p "$(dirname "$ICON_PNG")"
rsvg-convert -w 256 -h 256 -o "$ICON_PNG" "app/assets/$APP_ID.svg"
# appimagetool makes .DirIcon from the root-level icon named after the desktop file's Icon=,
# preferring a PNG; linuxdeploy only puts the SVG there, so add the PNG next to it.
ln -s "usr/share/icons/hicolor/256x256/apps/$APP_ID.png" "$APPDIR/$APP_ID.png"
# The metainfo goes in under the legacy .appdata.xml name, which AppStream still reads: AppImageHub
# builds its catalog entry (description, screenshots) only from *appdata.xml, and its linter looks
# for no other name. The Flatpak and the .deb keep the current .metainfo.xml name.
METAINFO="$APPDIR/usr/share/metainfo/$APP_ID.appdata.xml"
install -Dm644 "$L10N_DIR/$APP_ID.metainfo.xml" "$METAINFO"
# appimagetool validates an .appdata.xml with appstreamcli, URL checks included, and fails on any
# warning, so an unreachable homepage would fail the build (LDAI_NO_APPSTREAM, below). Check it
# here without the network instead, where appstreamcli is installed.
if command -v appstreamcli >/dev/null 2>&1; then
    appstreamcli validate --no-net "$METAINFO" || die "AppStream metainfo does not validate"
fi
mkdir -p "$APPDIR/usr/share/locale"
cp -a "$L10N_DIR/locale/." "$APPDIR/usr/share/locale/"
install -Dm644 LICENSE "$APPDIR/usr/share/doc/$APP_NAME/LICENSE"

# GStreamer: core plugins ship in libgstreamer, so copying the whole plugin
# directory covers whatever plugin packages are installed (base/good/libav/...).
mkdir -p "$APPDIR/usr/lib/gstreamer-1.0"
cp -a "$GST_PLUGINS_SRC"/*.so "$APPDIR/usr/lib/gstreamer-1.0/"
if [ -n "$GST_SCANNER_SRC" ]; then
    install -Dm755 "$GST_SCANNER_SRC" "$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
else
    warn "gst-plugin-scanner not found; registry scanning will run in-process"
fi

# gio modules: TLS (glib-networking) and the dconf GSettings backend.
mkdir -p "$APPDIR/usr/lib/gio/modules"
for module in libgiognutls.so libgiolibproxy.so libgiognomeproxy.so libdconfsettings.so; do
    if [ -f "$GIO_MODULES_SRC/$module" ]; then
        cp -a "$GIO_MODULES_SRC/$module" "$APPDIR/usr/lib/gio/modules/"
    else
        warn "gio module not found: $module"
    fi
done

# gdk-pixbuf loaders (png/jpeg/svg covers): copy the loaders and generate the
# cache with bare file names, then symlink the loaders into usr/lib so the
# names resolve through LD_LIBRARY_PATH (same trick as linuxdeploy-plugin-gtk;
# non-relocatable gdk-pixbuf builds resolve relative cache paths via dlopen).
PIXBUF_CACHE="$APPDIR/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
PIXBUF_MODULES_DST="$APPDIR/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders"
mkdir -p "$PIXBUF_MODULES_DST"
cp -a "$PIXBUF_MODULES_SRC"/*.so "$PIXBUF_MODULES_DST/"
(cd "$PIXBUF_MODULES_DST" && "$PIXBUF_QUERY" ./*.so) \
    | sed -e "s|$PIXBUF_MODULES_DST/||g" -e 's|\./||g' > "$PIXBUF_CACHE"
for loader in "$PIXBUF_MODULES_DST"/*.so; do
    ln -sf "gdk-pixbuf-2.0/2.10.0/loaders/$(basename "$loader")" "$APPDIR/usr/lib/"
done

# GTK4 modules (immodules, print backends); found via GTK_PATH at runtime.
if [ -d "$GTK4_MODULES_SRC" ]; then
    cp -a "$GTK4_MODULES_SRC" "$APPDIR/usr/lib/gtk-4.0"
    # The app never prints, and the CUPS backend is the only thing pulling in libcups (and its
    # avahi tail) — whose bookworm build is also the only library needing glibc 2.36, newer
    # than the 2.35 everything else needs (Ubuntu 22.04).
    find "$APPDIR/usr/lib/gtk-4.0" -name 'libprintbackend-cups.so' -delete
else
    warn "GTK4 module dir not found at $GTK4_MODULES_SRC; skipping"
fi

# glib settings schemas, compiled against the AppDir copy.
mkdir -p "$APPDIR/usr/share/glib-2.0"
cp -a "$SCHEMAS_SRC" "$APPDIR/usr/share/glib-2.0/schemas"
glib-compile-schemas "$APPDIR/usr/share/glib-2.0/schemas"

# Icon theme: the app uses -symbolic icons from Adwaita; ship the theme and
# the hicolor index so fallback lookups work without /usr/share on the host.
if [ -d /usr/share/icons/Adwaita ]; then
    cp -a /usr/share/icons/Adwaita "$APPDIR/usr/share/icons/Adwaita"
else
    warn "Adwaita icon theme not found; UI icons will depend on the host"
fi
if [ -f /usr/share/icons/hicolor/index.theme ]; then
    cp -a /usr/share/icons/hicolor/index.theme "$APPDIR/usr/share/icons/hicolor/index.theme"
fi

# The part of the launcher hook that points GStreamer at its plugins, in a file of its own so it
# can be tested (scripts/test-appimage-gst-env.sh); the hook below sources it.
install -Dm755 "$REPO_ROOT/scripts/appimage-gst-env.sh" "$APPDIR/usr/share/$APP_NAME/gst-env.sh"

# Runtime environment: sourced by linuxdeploy's default AppRun.
mkdir -p "$APPDIR/apprun-hooks"
cat > "$APPDIR/apprun-hooks/$APP_NAME.sh" <<'EOF'
#!/usr/bin/env bash
# The AppImage runtime sets APPDIR; an extracted AppImage (squashfs-root/AppRun, the usual
# workaround without FUSE) doesn't, and this file sits one level below the AppDir root.
APPDIR="${APPDIR:-"$(readlink -f "$(dirname "${BASH_SOURCE[0]}")/..")"}"
export APPDIR

# Everything below points the app at the bundle — but apps it launches on the host (the log
# file's text editor, the website's browser) must not inherit that, or they load our libraries
# instead of their own and die ("mousepad: symbol lookup error: ... g_once_init_leave_pointer").
# Record the host's values first (AUDIOBOOKLET_HOST_<VAR>, absent if the variable was unset);
# app/src/external_apps.rs puts them back for those launches. Kept from an earlier run of this
# hook, if any, so the values recorded are always the host's.
if [ -z "${AUDIOBOOKLET_BUNDLE_ENV+x}" ]; then
    export AUDIOBOOKLET_BUNDLE_ENV="LD_LIBRARY_PATH GTK_PATH GTK_EXE_PREFIX GSETTINGS_SCHEMA_DIR GDK_PIXBUF_MODULE_FILE GDK_PIXBUF_MODULEDIR GIO_MODULE_DIR GST_PLUGIN_SYSTEM_PATH GST_REGISTRY GST_REGISTRY_FORK GST_PLUGIN_SCANNER XDG_DATA_DIRS"
    for var in $AUDIOBOOKLET_BUNDLE_ENV; do
        if [ -n "${!var+x}" ]; then export "AUDIOBOOKLET_HOST_$var=${!var}"; fi
    done
fi

export LD_LIBRARY_PATH="$APPDIR/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export GTK_PATH="$APPDIR/usr/lib/gtk-4.0"
export GTK_EXE_PREFIX="$APPDIR/usr" # keep module lookups (immodules, media) inside the bundle
export GSETTINGS_SCHEMA_DIR="$APPDIR/usr/share/glib-2.0/schemas"
export GDK_PIXBUF_MODULE_FILE="$APPDIR/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"
export GDK_PIXBUF_MODULEDIR="$APPDIR/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders"
export GIO_MODULE_DIR="$APPDIR/usr/lib/gio/modules"
# The plugin folder and cache file (GST_PLUGIN_SYSTEM_PATH, GST_REGISTRY): see gst-env.sh for why
# they aren't simply the bundle's own path.
. "$APPDIR/usr/share/audiobooklet/gst-env.sh"
audiobooklet_gst_setup "$APPDIR"
export GST_REGISTRY_FORK=no
if [ -x "$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner" ]; then
    export GST_PLUGIN_SCANNER="$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
fi
export XDG_DATA_DIRS="$APPDIR/usr/share:/usr/share${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
EOF
chmod +x "$APPDIR/apprun-hooks/$APP_NAME.sh"

# --- let linuxdeploy resolve and bundle shared libraries, then package ---
export APPIMAGE_EXTRACT_AND_RUN=1 # run AppImage tooling without FUSE (containers)
export VERSION="$VERSION"         # read by the appimage output plugin

DEPLOY_ARGS=()
for d in "$APPDIR/usr/lib/gstreamer-1.0" "$APPDIR/usr/lib/gio/modules" \
    "$APPDIR/usr/lib/gdk-pixbuf-2.0" "$APPDIR/usr/lib/gtk-4.0"; do
    if [ -d "$d" ]; then
        DEPLOY_ARGS+=(--deploy-deps-only "$d")
    fi
done

# GStreamer's soup plugin (souphttpsrc — the only https:// source element) dlopen()s libsoup
# at runtime instead of linking it, so it's invisible to dependency resolution: nothing in the
# bundle has it as DT_NEEDED, and a host without a *glibc* libsoup-3 (or with none at all)
# gets "No URI handler implemented for https" and playback fails. Hand the library to
# linuxdeploy via --library so it and its own dependencies land in the bundle.
SOUP_LIB=""
for candidate in /usr/lib/*/libsoup-3.0.so.0 /usr/lib/libsoup-3.0.so.0; do
    if [ -e "$candidate" ]; then SOUP_LIB="$candidate"; break; fi
done
if [ -n "$SOUP_LIB" ]; then
    DEPLOY_ARGS+=(--library "$SOUP_LIB")
else
    # Hard failure: https:// playback is the app's whole purpose. (In practice this fires when
    # the build environment satisfied plugins-good's "libsoup2.4-1 OR libsoup-3.0-0" dependency
    # with soup2 — install libsoup-3.0-0 explicitly, the Dockerfile/CI do.)
    die "libsoup-3 not found in the build environment; https:// playback would be broken"
fi

# HarfBuzz is on the AppImage excludelist that linuxdeploy follows, so by default the bundled
# pango (from the build distro) runs against the host's HarfBuzz — and bookworm's pango 1.50
# needs HarfBuzz >= 4.0, so on Ubuntu 22.04 (2.7.4) and the like the app died at startup with
# "undefined symbol: hb_ot_layout_get_horizontal_baseline_tag_for_script". --library deploys it
# regardless of the excludelist. (Its own dependencies — glib, graphite2 — are bundled already;
# freetype stays the host's, and 22.04's is new enough.)
HARFBUZZ_LIB=""
for candidate in /usr/lib/*/libharfbuzz.so.0 /usr/lib/libharfbuzz.so.0; do
    if [ -e "$candidate" ]; then HARFBUZZ_LIB="$candidate"; break; fi
done
[ -n "$HARFBUZZ_LIB" ] || die "libharfbuzz.so.0 not found in the build environment"
DEPLOY_ARGS+=(--library "$HARFBUZZ_LIB")

OUTPUT_PATH="$OUTPUT_DIR/$APP_NAME-$VERSION-$ARCH.AppImage"

# The glob must match the *final* (post-rename, below) filename this script publishes, since
# that's what actually lands on the GitHub release AppImageUpdate resolves this against.
export LDAI_UPDATE_INFORMATION="gh-releases-zsync|$GH_OWNER|$GH_REPO|latest|$APP_NAME-*-$ARCH.AppImage.zsync"
# The metainfo was validated offline above.
export LDAI_NO_APPSTREAM=1

(
    cd "$OUTPUT_DIR"
    "$LINUXDEPLOY_BIN" \
        --appdir="$APPDIR" \
        --executable="$APPDIR/usr/bin/$APP_NAME" \
        --desktop-file="$APPDIR/usr/share/applications/$APP_ID.desktop" \
        --icon-file="$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg" \
        --icon-file="$ICON_PNG" \
        "${DEPLOY_ARGS[@]}" \
        --output=appimage
)

# The appimage plugin derives the output file name from the desktop file's
# Name (e.g. "Audiobooklet-<ver>-<arch>.AppImage"); normalize to ours. The
# output dir was cleared beforehand, so exactly one file can be present.
produced=""
for f in "$OUTPUT_DIR"/*.AppImage; do
    [ -f "$f" ] || continue
    if [ -n "$produced" ]; then
        die "multiple AppImages produced in $OUTPUT_DIR"
    fi
    produced="$f"
done
[ -n "$produced" ] || die "no AppImage produced in $OUTPUT_DIR"

# LDAI_UPDATE_INFORMATION above makes the appimage plugin shell out to zsyncmake and drop
# "<produced AppImage name>.zsync" next to it — same rename need as the AppImage itself.
produced_zsync="$produced.zsync"
[ -f "$produced_zsync" ] || die "no .zsync produced alongside $produced (is zsyncmake on PATH?)"

# linuxdeploy can silently ignore a --library input it doesn't like; the soup
# plugin dlopens libsoup, so nothing else in the bundle references it and its
# absence is invisible to every other check. Verify it landed.
[ -e "$APPDIR/usr/lib/libsoup-3.0.so.0" ] \
    || die "libsoup-3.0.so.0 did not land in the AppDir; https:// playback would be broken"
# Same for HarfBuzz: without it the app doesn't start on hosts with an older one.
[ -e "$APPDIR/usr/lib/libharfbuzz.so.0" ] \
    || die "libharfbuzz.so.0 did not land in the AppDir; the app wouldn't start on Ubuntu 22.04"

if [ "$produced" != "$OUTPUT_PATH" ]; then
    mv "$produced" "$OUTPUT_PATH"
    mv "$produced_zsync" "$OUTPUT_PATH.zsync"

    # zsyncmake wrote the .zsync file's Filename/URL header fields against the pre-rename name
    # (e.g. Audiobooklet-<ver>-<arch>.AppImage) — the name that's actually published is the one
    # we just renamed to. Left alone, a zsync client would resolve updates against a filename that
    # was never uploaded. Patch only the ASCII header (up to the first blank line); the checksum
    # block data that follows is binary and must not be touched.
    old_basename="$(basename "$produced")"
    new_basename="$(basename "$OUTPUT_PATH")"
    sed -i "0,/^\$/{s|^Filename: .*|Filename: $new_basename|; s|^URL: .*|URL: $new_basename|}" "$OUTPUT_PATH.zsync"
    if ! grep -q "^Filename: $new_basename\$" "$OUTPUT_PATH.zsync"; then
        die "failed to rewrite $OUTPUT_PATH.zsync's header from $old_basename to $new_basename"
    fi
fi

echo
echo "Built $OUTPUT_PATH ($(du -h "$OUTPUT_PATH" | cut -f1))"
sha256sum "$OUTPUT_PATH"
echo "Built $OUTPUT_PATH.zsync (for AppImageUpdate/Gear Lever)"
sha256sum "$OUTPUT_PATH.zsync"
