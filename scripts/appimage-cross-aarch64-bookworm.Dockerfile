# Cross-compile environment for scripts/build-aarch64-appimage-on-x86-fast.sh.
#
# A native x86_64 Debian bookworm image that compiles the Rust app for
# aarch64-unknown-linux-gnu against bookworm's *arm64* GTK4/libadwaita/GStreamer
# dev packages (Debian multiarch), so the library versions and glibc baseline are
# the same as in the arm64 image/CI build. Only the compiler runs natively; the
# resulting binary is then packaged in the (emulated) arm64 builder image.

FROM debian:bookworm

ENV DEBIAN_FRONTEND=noninteractive

# The -dev packages depend on gir1.2-*:arm64 typelib packages, which drag in
# gobject-introspection:arm64 (needs python3:arm64, not co-installable with the amd64
# python3). Nothing here needs GObject introspection, so satisfy those dependencies with
# empty arm64 stub packages (equivs) at the exact candidate version.
ARG GIR_STUBS="gir1.2-gtk-4.0 gir1.2-adw-1 gir1.2-gstreamer-1.0 gir1.2-gst-plugins-base-1.0 gir1.2-gdkpixbuf-2.0"
RUN dpkg --add-architecture arm64 && apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config git curl ca-certificates file equivs \
        gcc-aarch64-linux-gnu libc6-dev-arm64-cross libglib2.0-dev-bin \
    && mkdir /tmp/stubs && cd /tmp/stubs \
    && for p in $GIR_STUBS; do \
         v="$(apt-cache policy "$p:arm64" | sed -n 's/^ *Candidate: //p')"; \
         printf 'Section: misc\nPackage: %s\nVersion: %s\nArchitecture: arm64\nDescription: stub\n' "$p" "$v" > "$p.ctl"; \
         equivs-build --arch=arm64 "$p.ctl" >/dev/null; \
       done \
    && dpkg -i --force-architecture ./*.deb \
    && apt-get install -y --no-install-recommends \
        libgtk-4-dev:arm64 libadwaita-1-dev:arm64 \
        libgstreamer1.0-dev:arm64 libgstreamer-plugins-base1.0-dev:arm64 \
        libssl-dev:arm64 libpulse-dev:arm64 \
        libgdk-pixbuf-2.0-dev:arm64 \
    && rm -rf /var/lib/apt/lists/* /tmp/stubs

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal \
        --target aarch64-unknown-linux-gnu

# Target-scoped, so build scripts and proc-macros (host) keep using the native toolchain.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_LIBDIR_aarch64_unknown_linux_gnu=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
    PKG_CONFIG_SYSROOT_DIR_aarch64_unknown_linux_gnu=/
