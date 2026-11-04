#!/usr/bin/env bash
# Tests the two Docker wrappers (build-appimage-in-docker.sh, build-aarch64-appimage-on-x86.sh)
# against a fake `docker` that only records what it is asked to do: that the `docker run` that
# starts the build is one complete command (an image, then the build script), and carries the
# options the GStreamer plugin cache needs. No Docker daemon, no network, no build.
#
#   scripts/test-build-wrappers.sh
set -u

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAILED=0
check() { # check "what" <command...>
    local what="$1"
    shift
    if "$@"; then echo "ok    $what"; else echo "FAIL  $what"; FAILED=1; fi
}

mkdir -p "$TMP/bin" "$TMP/cache"
cat > "$TMP/bin/docker" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FAKE_DOCKER_LOG"
# The wrappers ask the architecture of the image they run (`... uname -m`).
case " $* " in *" uname "*) echo aarch64 ;; esac
exit 0
FAKE
chmod +x "$TMP/bin/docker"

run_wrapper() { # <wrapper> -> sets OUT, STATUS; the fake's log is $TMP/<name>.log
    local name
    name="$(basename "$1" .sh)"
    : > "$TMP/$name.log"
    OUT="$(cd "$REPO_ROOT" && PATH="$TMP/bin:$PATH" FAKE_DOCKER_LOG="$TMP/$name.log" XDG_CACHE_HOME="$TMP/cache" bash "$1" 2>&1)"
    STATUS=$?
    BUILD_RUN="$(grep -E '^run .*build-appimage\.sh' "$TMP/$name.log" | tail -n 1)"
}

check_wrapper() { # <script file> <description>
    run_wrapper "$REPO_ROOT/scripts/$1"
    check "$2 runs to the end" test "$STATUS" -eq 0
    if [ "$STATUS" -ne 0 ]; then
        echo "      $2 exited with $STATUS; its output:"
        printf '%s\n' "$OUT" | sed 's/^/      | /'
        echo "      what the fake docker was asked:"
        sed 's/^/      > /' "$TMP/$(basename "$1" .sh).log"
    fi
    check "$2 ran no stray command (a broken line continuation)" bash -c '! grep -qE "command not found|requires at least 1 argument" <<<"$1"' _ "$OUT"
    check "$2 starts the build in a container" test -n "$BUILD_RUN"
    check "$2 names an image, then the build script" grep -qE 'audiobooklet-appimage-builder[^ ]* \./scripts/build-appimage\.sh' <<<"$BUILD_RUN"
    check "$2 gives the build user a writable /run/user/1000" grep -qE -- '--tmpfs /run/user/1000:rw,mode=0700,uid=[0-9]+,gid=[0-9]+' <<<"$BUILD_RUN"
    check "$2 makes the build fail without the GStreamer cache" grep -qF -- '-e AUDIOBOOKLET_REQUIRE_GST_PREWARM=1' <<<"$BUILD_RUN"
    check "$2 keeps running as the host user" grep -qE -- '--user [0-9]+:[0-9]+' <<<"$BUILD_RUN"
}

check_wrapper build-appimage-in-docker.sh "the x86_64 wrapper"
if [ "$(uname -m)" = "x86_64" ]; then
    check_wrapper build-aarch64-appimage-on-x86.sh "the aarch64 wrapper"
else
    echo "SKIP: the aarch64 wrapper only runs on x86_64 hosts"
fi

if [ "$FAILED" -ne 0 ]; then
    echo "FAILED"
    exit 1
fi
echo "all passed"
