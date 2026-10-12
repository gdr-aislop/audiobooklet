# Build environment for scripts/build-appimage.sh (Debian bookworm).
#
# Used by scripts/build-appimage-in-docker.sh so local builds happen against
# the same library baseline as the CI builds (.github/workflows/release.yml,
# which runs the same apt install inside a debian:bookworm container job) and
# therefore against the same library versions as the target distros
# (PureOS Crimson, Ubuntu 24.04).

FROM debian:bookworm

ENV DEBIAN_FRONTEND=noninteractive
# libsoup-3.0-0 must be explicit: gstreamer1.0-plugins-good depends on
# "libsoup2.4-1 OR libsoup-3.0-0", and apt happily satisfies that with soup2 —
# leaving no libsoup-3 for GStreamer's soup plugin to dlopen at runtime, which
# kills all https:// playback in the produced AppImage.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config git curl ca-certificates file patchelf zsync gettext \
        libgtk-4-dev libadwaita-1-dev \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
        libssl-dev \
        libgdk-pixbuf-2.0-dev \
        glib-networking dconf-gsettings-backend gsettings-desktop-schemas \
        librsvg2-common librsvg2-bin adwaita-icon-theme \
        gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
        gstreamer1.0-libav gstreamer1.0-pulseaudio \
        libsoup-3.0-0 \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal
