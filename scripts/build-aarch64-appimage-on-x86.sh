#!/usr/bin/env bash
#
# Build an aarch64 AppImage on an x86_64 host using Docker's multi-platform
# support: everything runs in an arm64 Debian container under QEMU user-mode
# emulation (binfmt), so the Rust app and all AppImage tooling are native
# arm64 binaries and the output embeds the aarch64 AppImage runtime.
#
# Why not just download linuxdeploy like scripts/build-appimage.sh does on
# x86_64? Upstream only publishes the tooling as AppImages, which don't run
# inside Docker containers (no FUSE, and self-extraction is unreliable under
# QEMU). scripts/appimage-builder-bookworm-arm64.Dockerfile therefore
# compiles linuxdeploy, its appimage output plugin and appimagetool from
# source for arm64, and ships the aarch64 type-2 runtime file.
#
# Usage: scripts/build-aarch64-appimage-on-x86.sh [--version V] ...
# (all arguments are passed to scripts/build-appimage.sh --arch aarch64)
#
# Note: the first image build compiles three C++ projects under emulation and
# takes a while; later runs only re-run changed layers.

set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

die() { echo "error: $*" >&2; exit 1; }

# --- host preflight checks ---

HOST_ARCH="$(uname -m)"
if [ "$HOST_ARCH" != "x86_64" ]; then
    die "this script is for building aarch64 AppImages on x86_64 hosts (detected: $HOST_ARCH)"
fi

ENGINE=""
for candidate in docker podman; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" info >/dev/null 2>&1; then
        ENGINE="$candidate"
        break
    fi
done
[ -n "$ENGINE" ] || die "no working docker or podman found (is the daemon running?)"

# Verify multi-platform support (QEMU emulation)
if ! "$ENGINE" run --rm --platform linux/arm64 alpine uname -m 2>/dev/null | grep -q aarch64; then
    die "QEMU aarch64 emulation not available. Install binfmt support: $ENGINE run --privileged --rm tonistiigi/binfmt --install arm64"
fi

# --- configuration ---

IMAGE="audiobooklet-appimage-builder-bookworm:arm64"
DOCKERFILE="scripts/appimage-builder-bookworm-arm64.Dockerfile"
LOG_FILE="$REPO_ROOT/build/appimage/build-aarch64.log"
mkdir -p "$(dirname "$LOG_FILE")"

CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/audiobooklet-appimage-build"
mkdir -p "$CACHE_DIR/cargo"

REPO_MOUNT="$REPO_ROOT:/repo"
# SELinux hosts (Fedora/RHEL with podman) need relabeling for the bind mount.
if [ "$ENGINE" = "podman" ] && command -v getenforce >/dev/null 2>&1 \
    && [ "$(getenforce 2>/dev/null || true)" = "Enforcing" ]; then
    REPO_MOUNT="$REPO_MOUNT:Z"
fi

run_containerized_build() {
    echo "=== [$ENGINE build arm64] $(date -u '+%Y-%m-%d %H:%M:%SZ') ==="
    # The image build must succeed before anything runs: on failure the tag
    # keeps pointing at a stale image, which once silently produced an x86_64
    # AppDir inside the "arm64" run.
    if ! "$ENGINE" build --platform linux/arm64 -t "$IMAGE" -f "$DOCKERFILE" .; then
        die "builder image build failed"
    fi

    # Belt-and-braces: verify the tag really resolves to an arm64 image.
    image_arch="$("$ENGINE" run --rm --platform linux/arm64 --entrypoint uname "$IMAGE" -m 2>/dev/null || true)"
    if [ "${image_arch:-<run failed>}" != "aarch64" ]; then
        die "builder image is not arm64 (uname -m: ${image_arch:-<run failed>}); refusing to package with it"
    fi

    echo "=== [$ENGINE run arm64] ./scripts/build-appimage.sh --arch aarch64 $* ==="
    # The GStreamer plugin cache that ships in the AppImage is built for a path under
    # /run/user/1000 (scripts/appimage-gst-registry.sh): the unprivileged build user needs it
    # writable (--tmpfs), and the build must fail rather than ship without the cache.
    "$ENGINE" run --rm \
        --platform linux/arm64 \
        --user "$(id -u):$(id -g)" \
        --tmpfs "/run/user/1000:rw,mode=0700,uid=$(id -u),gid=$(id -g)" \
        -e AUDIOBOOKLET_REQUIRE_GST_PREWARM=1 \
        -e HOME=/tmp \
        -e CARGO_HOME=/cargo-home \
        -e CARGO_TARGET_DIR=/repo/build/appimage-target-arm64 \
        -e LINUXDEPLOY=/usr/local/bin/linuxdeploy \
        -e LDAI_RUNTIME_FILE=/usr/local/share/appimage/runtime-aarch64 \
        -w /repo \
        -v "$REPO_MOUNT" \
        -v "$CACHE_DIR/cargo:/cargo-home" \
        "$IMAGE" \
        ./scripts/build-appimage.sh --arch aarch64 "$@"
}

# Tee everything to the log; pipefail propagates the container's exit status
# through tee. set -e is suspended so a failing build still reaches the
# epilogue (and the log records the exit code before we exit with it).
set +e
run_containerized_build "$@" 2>&1 | tee "$LOG_FILE"
status=$?
set -e

echo "=== exit: $status (log: $LOG_FILE) ===" >&2
exit "$status"
