# Build environment for aarch64 AppImage builds on x86_64 hosts, used by
# scripts/build-aarch64-appimage-on-x86.sh:
#
#   docker build --platform linux/arm64 -t audiobooklet-appimage-builder-bookworm:arm64 \
#       -f scripts/appimage-builder-bookworm-arm64.Dockerfile .
#
# Every stage inherits the --platform passed to `docker build` (there are no
# per-stage --platform overrides on purpose — that is exactly how an
# x86_64-only tool ended up inside an arm64 image before). Under QEMU
# emulation the whole image, including the tools below, is native arm64:
#
#   - linuxdeploy and its appimage output plugin are compiled from source,
#     because upstream only ships them as AppImages, which don't run in
#     Docker containers (no FUSE, and self-extraction is unreliable under
#     QEMU emulation).
#   - appimagetool is compiled from source for the same reason, and because
#     the prebuilt one embeds an x86_64 AppImage runtime — the runtime glued
#     in front of the squashfs must be aarch64 for the result to work on
#     arm64. It is built dynamically; its runtime libraries are installed in
#     the final stage.
#   - mksquashfs is compiled from source because Debian bookworm ships 4.5.1,
#     but appimagetool always passes `-offset <runtime size>`, which
#     squashfs-tools only supports since 4.6 (same recipe as upstream's own
#     ci/install-static-mksquashfs.sh).

FROM debian:bookworm AS tool-builder

ARG LINUXDEPLOY_VERSION=1-alpha-20251107-1
ARG LINUXDEPLOY_PLUGIN_VERSION=1-alpha-20250213-1
ARG SQUASHFS_TOOLS_VERSION=4.6.1

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config git curl wget ca-certificates file patchelf \
        cmake ninja-build \
        libarchive-dev libglib2.0-dev \
        libgpgme-dev libgcrypt20-dev libcurl4-openssl-dev \
        libpng-dev \
        libjpeg-dev \
        libzstd-dev \
    && rm -rf /var/lib/apt/lists/*
# libpng-dev/libjpeg-dev: linuxdeploy's FindCImg requires both via
# pkg-config at configure time (the fat final image only gets them
# transitively through libgtk-4-dev).

WORKDIR /build

# linuxdeploy. patchelf is required at build time (find_program) and at run
# time; cmake configure downloads the ld.so excludelist, hence curl.
RUN git clone --depth 1 --branch "$LINUXDEPLOY_VERSION" https://github.com/linuxdeploy/linuxdeploy.git && \
    git -C linuxdeploy submodule update --init --recursive && \
    cmake -S linuxdeploy -B linuxdeploy/build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local -G Ninja && \
    cmake --build linuxdeploy/build -j"$(nproc)" && \
    cmake --install linuxdeploy/build

# appimage output plugin (found and exec'd by linuxdeploy via --output=appimage)
RUN git clone --depth 1 --branch "$LINUXDEPLOY_PLUGIN_VERSION" https://github.com/linuxdeploy/linuxdeploy-plugin-appimage.git && \
    git -C linuxdeploy-plugin-appimage submodule update --init --recursive && \
    cmake -S linuxdeploy-plugin-appimage -B linuxdeploy-plugin-appimage/build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local -G Ninja && \
    cmake --build linuxdeploy-plugin-appimage/build -j"$(nproc)" && \
    cmake --install linuxdeploy-plugin-appimage/build

# appimagetool, exec'd by the plugin to assemble the final AppImage. Upstream
# has no release tags; main is what their own CI builds. It shells out to
# mksquashfs (>= 4.6, built below) and desktop-file-validate.
RUN git clone --depth 1 https://github.com/AppImage/appimagetool.git && \
    cmake -S appimagetool -B appimagetool/build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local -G Ninja && \
    cmake --build appimagetool/build -j"$(nproc)" && \
    cmake --install appimagetool/build

# static mksquashfs 4.6.x into /usr/local/bin, shadowing bookworm's 4.5.1
RUN wget -qO- "https://github.com/plougher/squashfs-tools/archive/refs/tags/$SQUASHFS_TOOLS_VERSION.tar.gz" | tar xvz && \
    make -C "squashfs-tools-$SQUASHFS_TOOLS_VERSION/squashfs-tools" -j"$(nproc)" \
        GZIP_SUPPORT=0 XZ_SUPPORT=0 LZO_SUPPORT=0 LZ4_SUPPORT=0 ZSTD_SUPPORT=1 \
        COMP_DEFAULT=zstd LDFLAGS=-static USE_PREBUILT_MANPAGES=y install

FROM debian:bookworm

ENV DEBIAN_FRONTEND=noninteractive

# Same library baseline as scripts/appimage-builder-bookworm.Dockerfile (and
# .github/workflows/release.yml), plus the runtime deps of the tools built
# above: desktop-file-utils is exec'd by appimagetool; libarchive13,
# libdbus-1-3, libgpgme11 and libcurl4 are linked by linuxdeploy and
# appimagetool. mksquashfs comes from the tool-builder stage (bookworm's
# squashfs-tools is too old for appimagetool's -offset option).
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config git curl wget ca-certificates file patchelf zsync gettext \
        libgtk-4-dev libadwaita-1-dev \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
        libssl-dev \
        libgdk-pixbuf-2.0-dev \
        glib-networking dconf-gsettings-backend gsettings-desktop-schemas \
        librsvg2-common librsvg2-bin adwaita-icon-theme \
        gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
        gstreamer1.0-libav gstreamer1.0-pulseaudio \
        libsoup-3.0-0 \
        desktop-file-utils \
        libarchive13 libdbus-1-3 libgpgme11 libcurl4 \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal

COPY --from=tool-builder /usr/local/bin/linuxdeploy /usr/local/bin/linuxdeploy
COPY --from=tool-builder /usr/local/bin/linuxdeploy-plugin-appimage /usr/local/bin/linuxdeploy-plugin-appimage
COPY --from=tool-builder /usr/local/bin/appimagetool /usr/local/bin/appimagetool
COPY --from=tool-builder /usr/local/bin/mksquashfs /usr/local/bin/mksquashfs
COPY --from=tool-builder /usr/local/bin/unsquashfs /usr/local/bin/unsquashfs

# Type-2 AppImage runtime for aarch64; appimagetool glues it in front of the
# squashfs. Handed to the plugin via $LDAI_RUNTIME_FILE (set by the build
# wrapper); without it appimagetool would download a runtime at build time.
RUN mkdir -p /usr/local/share/appimage && \
    curl -fsSL --retry 3 -o /usr/local/share/appimage/runtime-aarch64 \
        https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-aarch64 && \
    chmod 0644 /usr/local/share/appimage/runtime-aarch64 && \
    [ "$(dd if=/usr/local/share/appimage/runtime-aarch64 bs=1 count=4 2>/dev/null)" = "$(printf '\177ELF')" ]
