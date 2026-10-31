#!/usr/bin/env bash
#
# Faster variant of scripts/build-aarch64-appimage-on-x86.sh.
#
# The original runs everything, including the ~450-crate `cargo build`, in an arm64
# container under QEMU. Here the Rust binary is cross-compiled *natively* on x86_64
# (scripts/appimage-cross-aarch64-bookworm.Dockerfile, against bookworm's arm64
# libraries), and only the packaging step (linuxdeploy/appimagetool, which must see
# arm64 libraries) runs in the emulated arm64 image from
# scripts/appimage-builder-bookworm-arm64.Dockerfile, via ABS_APP_BIN.
#
# Usage: scripts/build-aarch64-appimage-on-x86-fast.sh [--version V] ...
# (all arguments are passed to scripts/build-appimage.sh --arch aarch64)

set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

die() { echo "error: $*" >&2; exit 1; }

[ "$(uname -m)" = "x86_64" ] || die "this script is for x86_64 hosts (detected: $(uname -m))"

ENGINE=""
for candidate in docker podman; do
    if command -v "$candidate" >/dev/null 2>&1 && "$candidate" info >/dev/null 2>&1; then
        ENGINE="$candidate"
        break
    fi
done
[ -n "$ENGINE" ] || die "no working docker or podman found (is the daemon running?)"

if ! "$ENGINE" run --rm --platform linux/arm64 alpine uname -m 2>/dev/null | grep -q aarch64; then
    die "QEMU aarch64 emulation not available. Install binfmt support: $ENGINE run --privileged --rm tonistiigi/binfmt --install arm64"
fi

CROSS_IMAGE="audiobooklet-appimage-cross-bookworm:aarch64"
CROSS_DOCKERFILE="scripts/appimage-cross-aarch64-bookworm.Dockerfile"
IMAGE="audiobooklet-appimage-builder-bookworm:arm64"
DOCKERFILE="scripts/appimage-builder-bookworm-arm64.Dockerfile"
LOG_FILE="$REPO_ROOT/build/appimage/build-aarch64-fast.log"
mkdir -p "$(dirname "$LOG_FILE")"

CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/audiobooklet-appimage-build"
mkdir -p "$CACHE_DIR/cargo-cross" "$CACHE_DIR/cargo"

REPO_MOUNT="$REPO_ROOT:/repo"
if [ "$ENGINE" = "podman" ] && command -v getenforce >/dev/null 2>&1 \
    && [ "$(getenforce 2>/dev/null || true)" = "Enforcing" ]; then
    REPO_MOUNT="$REPO_MOUNT:Z"
fi

CROSS_TARGET_DIR="build/appimage-target-aarch64-cross"
BIN_REL="$CROSS_TARGET_DIR/aarch64-unknown-linux-gnu/release/audiobooklet"

run_build() {
    echo "=== [phase A: native cross-compile] $(date -u '+%Y-%m-%d %H:%M:%SZ') ==="
    "$ENGINE" build -t "$CROSS_IMAGE" -f "$CROSS_DOCKERFILE" . || die "cross image build failed"
    "$ENGINE" run --rm \
        --user "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/cargo-home \
        -e CARGO_TARGET_DIR="/repo/$CROSS_TARGET_DIR" \
        -w /repo \
        -v "$REPO_MOUNT" \
        -v "$CACHE_DIR/cargo-cross:/cargo-home" \
        "$CROSS_IMAGE" \
        cargo build --release --locked -p audiobooklet --target aarch64-unknown-linux-gnu \
        || die "cross compile failed"

    [ -f "$BIN_REL" ] || die "binary not found at $BIN_REL"
    # ELF e_machine (offset 18, little endian) must be 0xb7 = EM_AARCH64.
    [ "$(od -An -tx1 -j18 -N2 "$BIN_REL" | tr -d ' \n')" = "b700" ] \
        || die "$BIN_REL is not an aarch64 binary"

    echo "=== [phase B: arm64 packaging under QEMU] $(date -u '+%Y-%m-%d %H:%M:%SZ') ==="
    "$ENGINE" build --platform linux/arm64 -t "$IMAGE" -f "$DOCKERFILE" . || die "builder image build failed"
    image_arch="$("$ENGINE" run --rm --platform linux/arm64 --entrypoint uname "$IMAGE" -m 2>/dev/null || true)"
    [ "${image_arch:-<run failed>}" = "aarch64" ] \
        || die "builder image is not arm64 (uname -m: ${image_arch:-<run failed>})"

    "$ENGINE" run --rm \
        --platform linux/arm64 \
        --user "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -e CARGO_HOME=/cargo-home \
        -e ABS_APP_BIN="/repo/$BIN_REL" \
        -e LINUXDEPLOY=/usr/local/bin/linuxdeploy \
        -e LDAI_RUNTIME_FILE=/usr/local/share/appimage/runtime-aarch64 \
        -w /repo \
        -v "$REPO_MOUNT" \
        -v "$CACHE_DIR/cargo:/cargo-home" \
        "$IMAGE" \
        ./scripts/build-appimage.sh --arch aarch64 "$@"
    echo "=== [done] $(date -u '+%Y-%m-%d %H:%M:%SZ') ==="
}

set +e
run_build "$@" 2>&1 | tee "$LOG_FILE"
status=${PIPESTATUS[0]}
set -e

echo "=== exit: $status (log: $LOG_FILE) ===" >&2
exit "$status"
