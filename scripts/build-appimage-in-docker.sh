#!/usr/bin/env bash
#
# Run scripts/build-appimage.sh inside a Debian bookworm container, so local
# builds use the same library baseline as the CI builds (and the target
# distros). Arguments are passed through to the build script:
#
#   scripts/build-appimage-in-docker.sh [--arch x86_64|aarch64] [--version V] ...
#
# The first invocation pulls debian:bookworm and installs the dependencies
# into a locally cached image; later runs only re-run changed layers. The
# cargo download cache lives in ~/.cache/audiobooklet-appimage-build/cargo, and builds
# use a target dir separate from host builds (build/appimage-target).
#
# All output (image build + packaging run) is teed to build/appimage/build.log
# for post-mortem debugging — warnings like a missing runtime library are easy
# to miss in the live scroll.
#
# This wrapper is a developer convenience, not used by CI:
# .github/workflows/release.yml runs the build script directly inside its own
# debian:bookworm container job.

set -euo pipefail

# The repository's root: git's answer, or (a source tarball has no .git) the script's own parent.
REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null || (cd "$(dirname "$0")/.." && pwd))"
cd "$REPO_ROOT"

die() { echo "error: $*" >&2; exit 1; }

ENGINE=""
for candidate in docker podman; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" info >/dev/null 2>&1; then
        ENGINE="$candidate"
        break
    fi
done
[ -n "$ENGINE" ] || die "no working docker or podman found (is the daemon running?)"

IMAGE="audiobooklet-appimage-builder:bookworm"

LOG_FILE="$REPO_ROOT/build/appimage/build.log"
mkdir -p "$(dirname "$LOG_FILE")"
: > "$LOG_FILE"
echo "logging to $LOG_FILE" >&2

CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/audiobooklet-appimage-build"
mkdir -p "$CACHE_DIR/cargo"

REPO_MOUNT="$REPO_ROOT:/repo"
# SELinux hosts (Fedora/RHEL with podman) need relabeling for the bind mount.
if [ "$ENGINE" = "podman" ] && command -v getenforce >/dev/null 2>&1 \
    && [ "$(getenforce 2>/dev/null || true)" = "Enforcing" ]; then
    REPO_MOUNT="$REPO_MOUNT:Z"
fi

run_containerized_build() {
    echo "=== [$ENGINE build] $(date -u '+%Y-%m-%d %H:%M:%SZ') ==="
    "$ENGINE" build -t "$IMAGE" -f scripts/appimage-builder-bookworm.Dockerfile .
    echo "=== [$ENGINE run] ./scripts/build-appimage.sh $* ==="
    # The GStreamer plugin cache that ships in the AppImage is built for a path under
    # /run/user/1000 (scripts/appimage-gst-registry.sh): the unprivileged build user needs it
    # writable (--tmpfs), and the build must fail rather than ship without the cache.
    "$ENGINE" run --rm \
        --user "$(id -u):$(id -g)" \
        --tmpfs "/run/user/1000:rw,mode=0700,uid=$(id -u),gid=$(id -g)" \
        -e AUDIOBOOKLET_REQUIRE_GST_PREWARM=1 \
        -e HOME=/tmp \
        -e CARGO_HOME=/cargo-home \
        -e CARGO_TARGET_DIR=/repo/build/appimage-target \
        -w /repo \
        -v "$REPO_MOUNT" \
        -v "$CACHE_DIR/cargo:/cargo-home" \
        "$IMAGE" \
        ./scripts/build-appimage.sh "$@"
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
