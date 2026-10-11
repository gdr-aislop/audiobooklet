*This document was written by an LLM (Claude). Its steps were checked by building the app as
described (the Debian steps are the ones the release workflow runs; the Alpine steps were run in an
Alpine 3.24 x86_64 chroot), but double-check anything that matters before relying on it.*

# Building Audiobooklet from source

This covers building the `audiobooklet` binary itself. Packaged builds — `.deb`, AppImage,
Flatpak — are produced by the release workflow and described in the [README](README.md).

The result is a single binary, `target/release/audiobooklet`. It needs GTK 4.8+, libadwaita 1.2+,
GStreamer and PulseAudio (or PipeWire's PulseAudio server) at run time; the code is limited to the
GTK/libadwaita API that PureOS Crimson ships, so newer versions work too.

## PureOS Crimson, Debian 12 (bookworm) and compatible

These are the packages the release workflow installs to build the `.deb` on `debian:bookworm`
(Crimson's base):

```sh
sudo apt install build-essential pkg-config git curl ca-certificates gettext \
  libgtk-4-dev libadwaita-1-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libssl-dev libpulse-dev
```

To play audio you also need the GStreamer plugins and TLS support the `.deb` depends on:

```sh
sudo apt install gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-libav glib-networking
```

Rust comes from [rustup](https://rustup.rs) — Debian 12's own `rustc` (1.63) is too old for the
dependencies:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- --default-toolchain stable --profile minimal
. "$HOME/.cargo/env"
```

Then build and run:

```sh
git clone https://github.com/gdr-aislop/audiobooklet
cd audiobooklet
cargo build --release --locked -p audiobooklet
./target/release/audiobooklet
```

### Building on the phone itself

It works, but it is slow and needs a lot of memory: a release build takes about 28 CPU-minutes on a
modern x86 machine, and a phone CPU is several times slower. If the build is killed for lack of
memory, build with fewer parallel jobs (`cargo build --release --locked -p audiobooklet -j 2`) and/or
add swap.

## Translations (optional)

Without compiled catalogs the app runs in English. To use a translation from a source build, compile
the catalogs (needs `gettext`) and point the app at them:

```sh
scripts/build-l10n.sh build/l10n
ABS_LOCALEDIR=build/l10n/locale ./target/release/audiobooklet
```

The app also finds catalogs next to an installed binary, in `<binary dir>/../share/locale`. For
example, for a per-user install:

```sh
install -Dm755 target/release/audiobooklet ~/.local/bin/audiobooklet
mkdir -p ~/.local/share/locale && cp -r build/l10n/locale/. ~/.local/share/locale/
```

See [docs/i18n.md](docs/i18n.md) for how translations work.

## Alpine Linux and postmarketOS

postmarketOS is based on Alpine, and the same steps apply. Verified on Alpine 3.24 (x86_64);
postmarketOS itself, aarch64 and older Alpine branches were not tested, so package names or versions
may differ slightly there.

Build dependencies:

```sh
doas apk add build-base pkgconf git rust cargo \
  gtk4.0-dev libadwaita-dev gstreamer-dev gst-plugins-base-dev \
  openssl-dev pulseaudio-dev glib-dev
```

Run-time pieces for playback and TLS, plus `gettext` for translations and `gnome-keyring` for
"Remember password" (postmarketOS's Phosh images usually have the keyring already):

```sh
doas apk add gst-plugins-good gst-libav glib-networking gettext gnome-keyring
```

Use Alpine's own `rust` and `cargo` rather than rustup: rustup's musl targets link the C runtime
statically by default, which doesn't mix with the dynamically linked GTK stack. (If you must use
rustup, building with `RUSTFLAGS="-C target-feature=-crt-static"` should avoid that — not tested.)

Build and run exactly as on Debian:

```sh
git clone https://github.com/gdr-aislop/audiobooklet
cd audiobooklet
cargo build --release --locked -p audiobooklet
./target/release/audiobooklet
```

### Language on Alpine / postmarketOS

The app translates through GNU gettext (Alpine's `libintl`), so translations behave as on Debian,
with one thing to know: **`LANG` must name a real language**. With `LANG=pl_PL.UTF-8` the app is in
Polish, and Settings → Language can switch it to another language. With `LANG=C.UTF-8` (or no `LANG`
at all) gettext ignores the language choice, so the app stays in English whatever Settings says.
musl needs no locale generation — setting the variable is enough:

```sh
echo $LANG                                  # C.UTF-8 or empty? then:
LANG=en_US.UTF-8 ./target/release/audiobooklet
```

## Running the tests (optional)

The UI tests need a display, so run them under Xvfb (`xvfb` on Debian, `xvfb-run` on Alpine):

```sh
xvfb-run -a cargo test --workspace
```
