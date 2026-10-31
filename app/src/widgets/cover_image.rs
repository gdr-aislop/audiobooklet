//! A small reusable widget for a cover-art slot that starts as a plain placeholder and swaps to
//! the real image once one is cached locally (`abs_core::covers::fetch_and_cache_cover`). Not a
//! libadwaita compatibility shim like this module's other widgets — just a graceful-degradation
//! wrapper: a missing or corrupt cached file must never be treated as an error, only as "show the
//! placeholder", since cover art is cosmetic and this codebase already shows a plain placeholder
//! card everywhere covers aren't available yet.
//!
//! Decoding is asynchronous and cached process-wide. Library's search (and, more generally, any
//! screen that rebuilds many cards on every render/scroll) used to redecode every visible cover
//! from disk synchronously on the GTK main thread on every keystroke — the actual cause of a
//! reported multi-second UI freeze while typing. `set_path` now checks a shared, byte-budgeted
//! LRU texture cache first; on a miss it shows the placeholder immediately and decodes on a
//! background thread (`tokio::task::spawn_blocking`, resumed on the main thread once done via
//! `glib::spawn_future_local` — the same "start on the main loop, await a tokio-spawned task"
//! idiom `screens::library`'s own sync pipeline already uses), so a cache miss never blocks.
//!
//! The decode target is the widget's *physical* pixel size (`size` scaled by the surface's
//! `scale-factor`), not just its logical `size` — decoding at logical size only and letting a
//! HiDPI/scaled output (e.g. a phosh integer-scaled display) stretch it to fill the physical
//! surface blurs every cover uniformly, regardless of how small the slot is. See
//! `CoverImage::decode_pixels`.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::glib;
use adw::prelude::*;

use crate::i18n::tr;

/// Every cached texture is downscaled to exactly the requesting `CoverImage`'s `size`×`size` —
/// the source cover files are cached to disk at whatever resolution the server sent (no
/// resizing anywhere in `abs_core::covers`), and decoding them at native resolution into this
/// cache would be a real problem on a memory-constrained device (the largest size this app uses,
/// Player's full-screen cover, is 264px — a native-resolution book cover can easily be several
/// times that). Capped by *bytes*, not entry count: this cache is shared across every screen's
/// `CoverImage`, and different sizes cost wildly different amounts per entry, so an entry count
/// alone wouldn't give a real RAM ceiling.
const TEXTURE_CACHE_BUDGET_BYTES: usize = 24 * 1024 * 1024;

/// The texture cache's budget in low memory mode — a few covers, enough for the mini bar and the
/// player screen's big one.
const LOW_MEMORY_TEXTURE_BUDGET_BYTES: usize = 4 * 1024 * 1024;

/// How many cover decodes run at once. Bounds background CPU/thermal load when a burst of newly-
/// visible covers all become eligible to decode at once (e.g. a fast scroll) — the decode is
/// already off the main thread and thus non-blocking either way; this just stops it from
/// saturating every core on a mobile SoC. Mirrors `app/src/downloads.rs`'s `DownloadManager`
/// applying the same reasoning to concurrent track downloads.
const MAX_CONCURRENT_COVER_DECODES: usize = 3;

thread_local! {
    // `gdk::Texture` isn't `Send`, and GTK only ever runs on the main thread in this app, so a
    // `thread_local!` is the right tool here, not a `Mutex` — same reasoning already used
    // elsewhere in this crate for main-thread-only state. Only ever touched from the main
    // thread's own async continuations (see `set_path`), never from inside `spawn_blocking`.
    static TEXTURE_CACHE: RefCell<LruTextureCache> = RefCell::new(LruTextureCache::new(TEXTURE_CACHE_BUDGET_BYTES));
    /// Low memory mode: covers aren't decoded at all (see [`set_low_memory_mode`]).
    static LOW_MEMORY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Every live `CoverImage`, weakly, so switching the mode can update the ones already on
    /// screen. Pruned whenever it is walked and when it grows.
    static LIVE_COVERS: RefCell<Vec<WeakCover>> = const { RefCell::new(Vec::new()) };
    static DECODE_SEMAPHORE: Rc<tokio::sync::Semaphore> = Rc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_COVER_DECODES));
    /// What the covers decoding right now have cost so far; see [`DecodeBurst`].
    static DECODE_BURST: RefCell<DecodeBurst> = RefCell::new(DecodeBurst::default());
}

/// Where one cover decode spent its time, measured on the decode thread.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct DecodeTimings {
    /// Reading the cached file.
    read: Duration,
    /// Decoding the image (JPEG, PNG or WebP) at its full size.
    decode: Duration,
    /// Scaling to the slot's size and cropping.
    resize: Duration,
    source_width: u32,
    source_height: u32,
    file_bytes: u64,
}

/// The decodes of one burst (a screen's covers appearing, a scroll bringing new ones into view):
/// from the first one starting until none is left in flight. One log line per burst says how long
/// they took altogether, to decide whether covers are worth caching at the size they're shown at.
/// The per-phase times are summed over the decode threads (up to [`MAX_CONCURRENT_COVER_DECODES`]
/// at once), so they can add up to more than the burst's wall time.
#[derive(Debug, Default)]
struct DecodeBurst {
    started: Option<Instant>,
    in_flight: usize,
    decoded: usize,
    failed: usize,
    read: Duration,
    decode: Duration,
    resize: Duration,
    /// Turning the pixels into a texture and showing it, on the main thread. The upload to the
    /// graphics side happens later, when the texture is first painted, and isn't measured here.
    main_thread: Duration,
    file_bytes: u64,
    largest_source: (u32, u32),
    largest_target: i32,
    cache_hits: usize,
}

impl DecodeBurst {
    fn start_one(&mut self, now: Instant) {
        if self.in_flight == 0 {
            self.started = Some(now);
        }
        self.in_flight += 1;
    }

    /// One decode ended (`None`: it failed). Returns the summary line when it was the last of the
    /// burst, and starts the next burst's tally.
    fn finish_one(&mut self, result: Option<(DecodeTimings, Duration, i32)>, now: Instant) -> Option<String> {
        self.in_flight = self.in_flight.saturating_sub(1);
        match result {
            Some((timings, main_thread, target)) => {
                self.decoded += 1;
                self.read += timings.read;
                self.decode += timings.decode;
                self.resize += timings.resize;
                self.main_thread += main_thread;
                self.file_bytes += timings.file_bytes;
                if u64::from(timings.source_width) * u64::from(timings.source_height) > u64::from(self.largest_source.0) * u64::from(self.largest_source.1) {
                    self.largest_source = (timings.source_width, timings.source_height);
                }
                self.largest_target = self.largest_target.max(target);
            }
            None => self.failed += 1,
        }
        if self.in_flight > 0 || self.decoded + self.failed == 0 {
            return None;
        }
        let wall = self.started.map(|started| now.saturating_duration_since(started)).unwrap_or_default();
        let line = self.line(wall);
        *self = Self::default();
        Some(line)
    }

    fn line(&self, wall: Duration) -> String {
        format!(
            "covers decoded count={} failed={} wall_ms={} read_ms={} decode_ms={} resize_ms={} main_thread_ms={} source_kb={} largest_source={}x{} target_px={} cache_hits_before={}",
            self.decoded,
            self.failed,
            wall.as_millis(),
            self.read.as_millis(),
            self.decode.as_millis(),
            self.resize.as_millis(),
            self.main_thread.as_millis(),
            self.file_bytes / 1024,
            self.largest_source.0,
            self.largest_source.1,
            self.largest_target,
            self.cache_hits
        )
    }
}

/// Counts one decode in flight; when dropped it reports how it ended and logs the burst's summary
/// if it was the last. A guard so that no way out of the decode task (an error, a panic, the
/// future being dropped) can leave the count too high.
struct InFlightDecode {
    result: Option<(DecodeTimings, Duration, i32)>,
}

impl InFlightDecode {
    fn begin() -> Self {
        DECODE_BURST.with(|burst| burst.borrow_mut().start_one(Instant::now()));
        Self { result: None }
    }

    /// This decode succeeded; reported when the guard is dropped, at the end of the task.
    fn complete(&mut self, timings: DecodeTimings, main_thread: Duration, target: i32) {
        self.result = Some((timings, main_thread, target));
    }
}

impl Drop for InFlightDecode {
    fn drop(&mut self) {
        let line = DECODE_BURST.with(|burst| burst.borrow_mut().finish_one(self.result.take(), Instant::now()));
        if let Some(line) = line {
            tracing::info!("{line}");
            tracing::info!("{} {}", TEXTURE_CACHE.with(|cache| cache.borrow().report()), live_cover_report());
        }
    }
}

/// Keyed by `(path, size)`, not just `path` — different screens request different sizes for the
/// same item (a grid tile vs. a list thumbnail vs. Player's mini-bar vs. Item Detail's larger
/// cover), and a texture correctly sized for one use would be blurry upscaled for another.
struct LruTextureCache {
    budget_bytes: usize,
    used_bytes: usize,
    /// Access order, oldest first — touched (moved to the back) on every hit *and* insert, so
    /// the least-recently-shown entry is always what gets evicted first.
    order: VecDeque<(PathBuf, i32)>,
    entries: HashMap<(PathBuf, i32), gtk4::gdk::Texture>,
    stats: CacheStats,
    /// The last few entries thrown out, oldest first, so a decode of one of them again can be
    /// told from a first decode (see [`CacheStats::redecodes`]). Bounded by `EVICTED_MEMORY`.
    evicted: VecDeque<(PathBuf, i32)>,
}

/// How many evicted entries are remembered to recognise a re-decode.
const EVICTED_MEMORY: usize = 1024;

/// What the texture cache has done since the app started — logged with each decode burst, to see
/// from the phone whether the budget is too small for what is on screen instead of guessing.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
struct CacheStats {
    hits: u64,
    misses: u64,
    inserts: u64,
    evictions: u64,
    evicted_bytes: u64,
    /// Decodes of an entry this cache had already held and evicted: the direct measure of thrashing.
    redecodes: u64,
}

impl LruTextureCache {
    fn new(budget_bytes: usize) -> Self {
        Self { budget_bytes, used_bytes: 0, order: VecDeque::new(), entries: HashMap::new(), stats: CacheStats::default(), evicted: VecDeque::new() }
    }

    /// Drops the least recently used entry; false when there is none.
    fn evict_oldest(&mut self) -> bool {
        let Some(oldest) = self.order.pop_front() else { return false };
        if let Some(removed) = self.entries.remove(&oldest) {
            let bytes = texture_bytes(&removed);
            self.used_bytes = self.used_bytes.saturating_sub(bytes);
            self.stats.evictions += 1;
            self.stats.evicted_bytes += bytes as u64;
            self.evicted.push_back(oldest);
            while self.evicted.len() > EVICTED_MEMORY {
                self.evicted.pop_front();
            }
        }
        true
    }

    /// One line for the log: sizes, the counters, and how many entries there are of each size.
    fn report(&self) -> String {
        let mut by_size: std::collections::BTreeMap<i32, usize> = std::collections::BTreeMap::new();
        for (_, size) in self.entries.keys() {
            *by_size.entry(*size).or_default() += 1;
        }
        let sizes: Vec<String> = by_size.iter().map(|(size, count)| format!("{size}px:{count}")).collect();
        format!(
            "cover texture cache entries={} used_kb={} budget_kb={} hits={} misses={} inserts={} evictions={} evicted_kb={} redecodes={} entries_by_size=[{}]",
            self.entries.len(),
            self.used_bytes / 1024,
            self.budget_bytes / 1024,
            self.stats.hits,
            self.stats.misses,
            self.stats.inserts,
            self.stats.evictions,
            self.stats.evicted_bytes / 1024,
            self.stats.redecodes,
            sizes.join(",")
        )
    }

    /// Changes the budget and evicts (least recently used first) down to it.
    fn set_budget(&mut self, budget_bytes: usize) {
        self.budget_bytes = budget_bytes;
        while self.used_bytes > self.budget_bytes && self.evict_oldest() {}
    }

    fn get(&mut self, key: &(PathBuf, i32)) -> Option<gtk4::gdk::Texture> {
        let Some(texture) = self.entries.get(key).cloned() else {
            self.stats.misses += 1;
            return None;
        };
        self.stats.hits += 1;
        self.touch(key);
        Some(texture)
    }

    fn touch(&mut self, key: &(PathBuf, i32)) {
        if let Some(pos) = self.order.iter().position(|existing| existing == key) {
            let key = self.order.remove(pos).expect("position came from this same deque");
            self.order.push_back(key);
        }
    }

    /// A concurrent decode of the same `(path, size)` landing twice (e.g. two cards for the same
    /// item scrolled into view close together) just bumps recency the second time — no double
    /// accounting.
    fn insert(&mut self, key: (PathBuf, i32), texture: gtk4::gdk::Texture) {
        if self.entries.contains_key(&key) {
            self.touch(&key);
            return;
        }
        let byte_size = texture_bytes(&texture);
        if let Some(at) = self.evicted.iter().position(|gone| *gone == key) {
            self.evicted.remove(at);
            self.stats.redecodes += 1;
        }
        self.stats.inserts += 1;
        self.entries.insert(key.clone(), texture);
        self.order.push_back(key);
        self.used_bytes += byte_size;
        while self.used_bytes > self.budget_bytes && self.evict_oldest() {}
    }
}

/// How many `CoverImage` widgets exist, how many of them are on screen, and the display's scale
/// factor: with the cache's own numbers, tells "the cache is too small" from "far more covers are
/// built than are ever shown".
fn live_cover_report() -> String {
    let (mut live, mut mapped, mut scale) = (0usize, 0usize, 0i32);
    LIVE_COVERS.with(|covers| {
        for cover in covers.borrow().iter() {
            let Some(overlay) = cover.overlay.upgrade() else { continue };
            live += 1;
            if overlay.is_mapped() {
                mapped += 1;
            }
            scale = scale.max(overlay.scale_factor());
        }
    });
    format!("live_covers={live} mapped_covers={mapped} scale_factor={scale}")
}

fn texture_bytes(texture: &gtk4::gdk::Texture) -> usize {
    (texture.width() as usize) * (texture.height() as usize) * 4
}

/// Pure so it's unit-testable without a real, possibly-scaled display (Xvfb always reports scale
/// factor 1, so a GTK-level test can't exercise the HiDPI case at all). `scale_factor` is clamped
/// to at least 1 — GTK never reports less, but an unrealized widget's default shouldn't either.
fn decode_pixels_for(size: i32, scale_factor: i32) -> i32 {
    size * scale_factor.max(1)
}

/// A `CoverImage` held weakly: `None` from [`WeakCover::upgrade`] once its widgets are gone.
struct WeakCover {
    overlay: glib::WeakRef<gtk4::Overlay>,
    placeholder: glib::WeakRef<gtk4::Box>,
    picture: glib::WeakRef<gtk4::Picture>,
    last_path: std::rc::Weak<RefCell<Option<PathBuf>>>,
    on_activate: std::rc::Weak<RefCell<Option<OnActivate>>>,
    size: i32,
}

/// What a tap on a tappable cover does — see [`CoverImage::set_on_activate`].
type OnActivate = Rc<dyn Fn(Option<gtk4::gdk::Texture>, PathBuf)>;

impl WeakCover {
    fn upgrade(&self) -> Option<CoverImage> {
        Some(CoverImage {
            overlay: self.overlay.upgrade()?,
            placeholder: self.placeholder.upgrade()?,
            picture: self.picture.upgrade()?,
            last_path: self.last_path.upgrade()?,
            on_activate: self.on_activate.upgrade()?,
            size: self.size,
        })
    }
}

/// Turns low memory mode on or off for every cover: on, nothing is decoded any more — each
/// `CoverImage` shows its placeholder and the texture cache shrinks to
/// [`LOW_MEMORY_TEXTURE_BUDGET_BYTES`] (textures a screen still holds are released when it
/// rebuilds); off, covers decode again and the cache gets its full budget back.
pub fn set_low_memory_mode(on: bool) {
    if LOW_MEMORY.with(|flag| flag.replace(on)) == on {
        return;
    }
    TEXTURE_CACHE.with(|cache| cache.borrow_mut().set_budget(if on { LOW_MEMORY_TEXTURE_BUDGET_BYTES } else { TEXTURE_CACHE_BUDGET_BYTES }));
    // Re-run `set_path` for every cover on screen so the new mode applies to what is showing.
    let covers: Vec<CoverImage> = LIVE_COVERS.with(|live| {
        let mut live = live.borrow_mut();
        let upgraded: Vec<CoverImage> = live.iter().filter_map(WeakCover::upgrade).collect();
        live.retain(|weak| weak.picture.upgrade().is_some());
        upgraded
    });
    for cover in covers {
        let path = cover.last_path.borrow_mut().take();
        cover.set_path(path.as_deref());
    }
}

pub(crate) fn low_memory_mode() -> bool {
    LOW_MEMORY.with(|flag| flag.get())
}

#[derive(Clone)]
pub struct CoverImage {
    overlay: gtk4::Overlay,
    placeholder: gtk4::Box,
    picture: gtk4::Picture,
    /// The path whose image is currently requested, shared across clones (the widget handle and
    /// the closure it was cloned into must agree). `RefCell`, not `Cell` — `set_path`'s async
    /// decode continuation needs to *read* this without consuming it, to check it's still
    /// current before applying a possibly-stale result (see `set_path`'s staleness guard).
    last_path: Rc<RefCell<Option<PathBuf>>>,
    on_activate: Rc<RefCell<Option<OnActivate>>>,
    /// Both the widget's fixed width/height and the cache key's size component.
    size: i32,
}

impl CoverImage {
    /// `size` is both width and height — every cover slot in this app is square.
    pub fn new(size: i32) -> Self {
        // Every widget here is pinned to a fixed, non-expanding, non-stretching `size`x`size` box.
        // The sizing discipline below was learned the hard way once real covers actually started
        // loading (WebP took until the in-process decoder landed; see `set_path`):
        // 1. `GtkPicture` defaults to `hexpand`/`vexpand: true`, and with only one sibling in a
        //    shelf row (as Home's "Continue Listening" often has), nothing else claimed the
        //    leftover space, so the whole card stretched into a short, wide rectangle instead of
        //    staying square — `content-fit: Cover` then cropped the real cover into that
        //    wrong-aspect box, visibly distorting it.
        // 2. Fixing that (explicit `hexpand`/`vexpand: false`) still left the *overlay* itself at
        //    its default `halign`/`valign: Fill` — so it stayed square, but a single-item row
        //    could still allocate the whole card more width than its `size`x`size` natural size,
        //    and Fill alignment stretched the overlay (and the picture inside it) to match,
        //    rendering a *correctly square but uniformly larger* cover than the same widget gets
        //    in a row with enough siblings to fill the space (confirmed live, side by side: the
        //    same "Continue Listening" cover rendered visibly more zoomed-in than the identical
        //    item's card in "Recently Added"). `halign`/`valign: Center` on the overlay pins it to
        //    exactly its natural size regardless of how much extra space a parent offers it.
        let placeholder = gtk4::Box::builder().css_classes(["card"]).width_request(size).height_request(size).build();
        let picture = gtk4::Picture::builder()
            .width_request(size)
            .height_request(size)
            .content_fit(gtk4::ContentFit::Cover)
            .hexpand(false)
            .vexpand(false)
            .halign(gtk4::Align::Fill)
            .valign(gtk4::Align::Fill)
            .visible(false)
            .build();

        let overlay = gtk4::Overlay::builder()
            .child(&placeholder)
            .width_request(size)
            .height_request(size)
            .hexpand(false)
            .vexpand(false)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        overlay.add_overlay(&picture);

        let cover = Self { overlay, placeholder, picture, last_path: Rc::new(RefCell::new(None)), on_activate: Rc::new(RefCell::new(None)), size };
        LIVE_COVERS.with(|live| {
            let mut live = live.borrow_mut();
            if live.len() >= 256 {
                live.retain(|weak| weak.picture.upgrade().is_some());
            }
            live.push(WeakCover {
                overlay: cover.overlay.downgrade(),
                placeholder: cover.placeholder.downgrade(),
                picture: cover.picture.downgrade(),
                last_path: Rc::downgrade(&cover.last_path),
                on_activate: Rc::downgrade(&cover.on_activate),
                size,
            });
        });
        cover
    }

    pub fn widget(&self) -> &gtk4::Widget {
        self.overlay.upcast_ref()
    }

    /// Makes the cover tappable: a press dims and shrinks it a little at once (the finger gets an
    /// answer before anything loads — see `widgets::cover_viewer`), and a release on it calls
    /// `on_activate(texture shown now, cached cover path)`. A drag that turns into scrolling
    /// cancels the press. Inert while there is no cover path (a cover-less placeholder).
    pub fn set_on_activate(&self, on_activate: impl Fn(Option<gtk4::gdk::Texture>, PathBuf) + 'static) {
        *self.on_activate.borrow_mut() = Some(Rc::new(on_activate));
        ensure_cover_css();
        self.overlay.add_css_class("cover-tappable");
        self.overlay.set_cursor_from_name(Some("pointer"));
        self.overlay.set_tooltip_text(Some(&tr("View cover")));
        let click = gtk4::GestureClick::new();
        click.connect_pressed({
            let cover = self.clone();
            move |_, _, _, _| {
                if cover.last_path.borrow().is_some() {
                    cover.overlay.add_css_class("cover-pressed");
                }
            }
        });
        click.connect_stopped({
            let overlay = self.overlay.clone();
            move |_| overlay.remove_css_class("cover-pressed")
        });
        click.connect_released({
            let cover = self.clone();
            move |gesture, _, x, y| {
                cover.overlay.remove_css_class("cover-pressed");
                if !cover.overlay.contains(x, y) {
                    return;
                }
                if cover.last_path.borrow().is_none() {
                    return;
                }
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                cover.activate();
            }
        });
        self.overlay.add_controller(click);
    }

    /// Calls the tap callback with what's on screen — the release half of a tap.
    fn activate(&self) {
        let Some(path) = self.last_path.borrow().clone() else { return };
        let Some(on_activate) = self.on_activate.borrow().clone() else { return };
        let shown = self
            .picture
            .is_visible()
            .then(|| self.picture.paintable())
            .flatten()
            .and_then(|paintable| paintable.downcast::<gtk4::gdk::Texture>().ok());
        on_activate(shown, path);
    }

    /// A tap, for scenarios (no real pointer under Xvfb).
    #[cfg(test)]
    pub(crate) fn tap(&self) {
        self.activate();
    }

    /// The pixel resolution to actually decode at: `size` (logical) scaled by this widget's
    /// current physical scale factor, so a HiDPI/phosh-style scaled output gets a texture with
    /// enough real pixels to fill its physical surface without the compositor upscaling (and
    /// blurring) it. Falls back to 1x if unrealized (not yet attached to a surface) — decoding
    /// too small in that edge case just means a possible one-time re-decode later, never a crash.
    fn decode_pixels(&self) -> i32 {
        decode_pixels_for(self.size, self.overlay.scale_factor())
    }

    /// Test-only view of the underlying `GtkPicture` — scenarios assert visibility on it to pin,
    /// end to end, that a cached cover actually rendered rather than stayed a placeholder.
    #[cfg(test)]
    pub(crate) fn picture(&self) -> &gtk4::Picture {
        &self.picture
    }

    /// Shows the cached image at `path`, or falls back to the placeholder if `path` is `None` or
    /// the file can't be decoded (missing, corrupt, unsupported format — none of these should
    /// ever crash a screen over cosmetic art). Repeated calls with the same path are a no-op —
    /// needed both for the player's ~4Hz snapshot cadence (this widget instance is reused/
    /// republished on every tick) and, now, to avoid kicking off a redundant decode.
    ///
    /// Never blocks: a cache hit is an in-memory texture clone; a miss shows the placeholder
    /// immediately and decodes on a background thread, applying the result later if (and only
    /// if) this `CoverImage` is still showing the path that was requested.
    pub fn set_path(&self, path: Option<&Path>) {
        let previous = self.last_path.replace(path.map(Path::to_path_buf));
        if previous.as_deref() == path {
            return;
        }
        let Some(path) = path else {
            self.show_placeholder();
            return;
        };
        // Low memory mode: no covers. The path is remembered (above) so leaving the mode shows it.
        if low_memory_mode() {
            self.show_placeholder();
            return;
        }
        let decode_pixels = self.decode_pixels();
        let key = (path.to_path_buf(), decode_pixels);

        if let Some(texture) = TEXTURE_CACHE.with(|cache| cache.borrow_mut().get(&key)) {
            DECODE_BURST.with(|burst| burst.borrow_mut().cache_hits += 1);
            self.show_texture(texture);
            return;
        }

        self.show_placeholder();

        let picture = self.picture.clone();
        let placeholder = self.placeholder.clone();
        let last_path = self.last_path.clone();
        let requested = path.to_path_buf();
        let size = decode_pixels;
        let mut in_flight = InFlightDecode::begin();
        glib::spawn_future_local(async move {
            let semaphore = DECODE_SEMAPHORE.with(|semaphore| semaphore.clone());
            let _permit = semaphore.acquire().await.expect("cover-decode semaphore is never closed");

            let decoded = tokio::task::spawn_blocking({
                let requested = requested.clone();
                move || decode_and_crop_to_cover(&requested, size)
            })
            .await;

            let (bytes, side, timings) = match decoded {
                Ok(Ok(result)) => result,
                Ok(Err(err)) => {
                    tracing::warn!(%err, ?requested, "couldn't decode the cached cover image");
                    return;
                }
                Err(err) => {
                    tracing::warn!(%err, ?requested, "cover decode task panicked");
                    return;
                }
            };

            let on_main_thread = Instant::now();
            let texture: gtk4::gdk::Texture =
                gtk4::gdk::MemoryTexture::new(side as i32, side as i32, gtk4::gdk::MemoryFormat::R8g8b8a8, &gtk4::glib::Bytes::from_owned(bytes), (side * 4) as usize).into();

            TEXTURE_CACHE.with(|cache| cache.borrow_mut().insert((requested.clone(), size), texture.clone()));

            // Staleness guard: this `CoverImage` may have been rebound to a different path while
            // this decode was in flight (the player's mini-bar reuses one instance across ticks,
            // sometimes onto a genuinely new track) — only apply if nothing newer superseded it.
            if last_path.borrow().as_deref() == Some(requested.as_path()) {
                picture.set_paintable(Some(&texture));
                picture.set_visible(true);
                placeholder.set_visible(false);
            }
            in_flight.complete(timings, on_main_thread.elapsed(), size);
        });
    }

    fn show_texture(&self, texture: gtk4::gdk::Texture) {
        self.picture.set_paintable(Some(&texture));
        self.picture.set_visible(true);
        self.placeholder.set_visible(false);
    }

    fn show_placeholder(&self) {
        self.picture.set_visible(false);
        self.placeholder.set_visible(true);
    }
}

/// The pressed look of a tappable cover (see [`CoverImage::set_on_activate`]).
fn ensure_cover_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(
            "overlay.cover-pressed { opacity: 0.7; transform: scale(0.97); } \
             overlay.cover-tappable { transition: opacity 120ms ease-out, transform 120ms ease-out; }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("a display for the app's css"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

/// Decodes an image file (format sniffed from the content, so the cached file's extension can't
/// lie — needed for WebP, which gdk-pixbuf has no loader for on the target distros: no WebP
/// loader ships in Debian bookworm, PureOS Crimson's base, and Audiobookshelf serves plenty of
/// WebP covers) and scales+crops it to exactly `side`x`side`, matching `GtkContentFit::Cover`'s
/// aspect-preserving crop rather than a naive stretch — scaling up until both dimensions are at
/// least `side` (preserving aspect ratio), then center-cropping the overflow. Runs on a
/// `spawn_blocking` thread; returns plain, `Send`-safe bytes rather than a `gdk::Texture`
/// (a GObject) deliberately — constructing/touching one off the thread that owns the GTK main
/// context is exactly the kind of thing that can crash unpredictably under Wayland/EGL, so the
/// `gdk::MemoryTexture` itself is always built back on the main thread, in `set_path`.
fn decode_and_crop_to_cover(path: &Path, side: i32) -> Result<(Vec<u8>, u32, DecodeTimings), image::ImageError> {
    let side = side as u32;
    let reading = Instant::now();
    let bytes = std::fs::read(path)?;
    let read = reading.elapsed();
    let decoding = Instant::now();
    let decoded = image::load_from_memory(&bytes)?;
    let decode = decoding.elapsed();
    let resizing = Instant::now();
    let (width, height) = (decoded.width(), decoded.height());
    let scale = (side as f64 / width as f64).max(side as f64 / height as f64);
    let scaled_width = ((width as f64 * scale).ceil() as u32).max(side);
    let scaled_height = ((height as f64 * scale).ceil() as u32).max(side);
    let scaled = decoded.resize_exact(scaled_width, scaled_height, image::imageops::FilterType::Triangle);
    let x = (scaled_width - side) / 2;
    let y = (scaled_height - side) / 2;
    let cropped = scaled.crop_imm(x, y, side, side).into_rgba8();
    let timings = DecodeTimings { read, decode, resize: resizing.elapsed(), source_width: width, source_height: height, file_bytes: bytes.len() as u64 };
    Ok((cropped.into_raw(), side, timings))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::pump_until;
    use std::time::Duration;

    fn timings(read: u64, decode: u64, resize: u64, width: u32, height: u32, bytes: u64) -> DecodeTimings {
        DecodeTimings { read: Duration::from_millis(read), decode: Duration::from_millis(decode), resize: Duration::from_millis(resize), source_width: width, source_height: height, file_bytes: bytes }
    }

    /// One summary per burst, at the moment the last decode in flight ends, with the phases summed.
    #[test]
    fn a_burst_of_decodes_is_summarised_once_when_the_last_one_ends() {
        let t0 = Instant::now();
        let mut burst = DecodeBurst::default();
        burst.cache_hits = 7;
        for _ in 0..3 {
            burst.start_one(t0);
        }
        assert_eq!(burst.finish_one(Some((timings(5, 100, 20, 1200, 1200, 300 * 1024), Duration::from_millis(2), 264)), t0 + Duration::from_millis(150)), None);
        assert_eq!(burst.finish_one(None, t0 + Duration::from_millis(200)), None, "a failure counts as an ending too");
        let line = burst
            .finish_one(Some((timings(3, 80, 10, 600, 900, 100 * 1024), Duration::from_millis(1), 132)), t0 + Duration::from_millis(250))
            .expect("the last one ends the burst");
        assert_eq!(
            line,
            "covers decoded count=2 failed=1 wall_ms=250 read_ms=8 decode_ms=180 resize_ms=30 main_thread_ms=3 source_kb=400 largest_source=1200x1200 target_px=264 cache_hits_before=7"
        );
        // The next burst starts from nothing.
        burst.start_one(t0);
        let next = burst.finish_one(Some((timings(1, 2, 3, 10, 10, 1024), Duration::ZERO, 64)), t0 + Duration::from_millis(5)).unwrap();
        assert!(next.starts_with("covers decoded count=1 failed=0 wall_ms=5 ") && next.ends_with("cache_hits_before=0"), "{next}");
    }

    /// However a decode task ends (it failed, it finished, it was dropped before doing anything),
    /// the number in flight comes back down, so a burst can always end.
    #[test]
    fn the_guard_keeps_the_in_flight_count_right_however_the_task_ends() {
        let in_flight = || DECODE_BURST.with(|burst| burst.borrow().in_flight);
        assert_eq!(in_flight(), 0);
        {
            let _never_ran = InFlightDecode::begin();
            let mut done = InFlightDecode::begin();
            assert_eq!(in_flight(), 2);
            done.complete(DecodeTimings::default(), Duration::ZERO, 64);
        }
        assert_eq!(in_flight(), 0, "both ended, one successfully and one without a result");
        assert_eq!(DECODE_BURST.with(|burst| burst.borrow().decoded + burst.borrow().failed), 0, "and the tally was reset with the burst's summary");
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests` for why every fast GTK-touching
    /// scenario in this binary has to run from one single entry point.
    pub(crate) fn run(_runtime: &tokio::runtime::Runtime) {
        let cover = CoverImage::new(64);
        assert!(cover.placeholder.is_visible(), "starts showing the placeholder");
        assert!(!cover.picture.is_visible());

        cover.set_path(None);
        assert!(cover.placeholder.is_visible(), "no path keeps the placeholder");

        let tmp = tempfile::tempdir().unwrap();
        let corrupt = tmp.path().join("not-an-image.png");
        std::fs::write(&corrupt, b"not actually a png").unwrap();
        cover.set_path(Some(&corrupt));
        // A corrupt file fails inside the background decode — pump the main loop for a moment
        // (letting the spawned decode task actually resolve) and confirm it settles back on the
        // placeholder rather than panicking or hanging.
        pump_until(|| false, Duration::from_millis(200));
        assert!(cover.placeholder.is_visible(), "a corrupt file should fall back to the placeholder, not panic");
        assert!(!cover.picture.is_visible());

        let valid = tmp.path().join("real.png");
        write_1x1_png(&valid);
        cover.set_path(Some(&valid));
        pump_until(|| cover.picture.is_visible(), Duration::from_secs(5));
        assert!(!cover.placeholder.is_visible());

        // WebP has no gdk-pixbuf loader on the target distros — the in-process fallback must
        // pick it up (regression guard for the audiobookshelf-client cover pipeline).
        let webp = tmp.path().join("real.webp");
        write_1x1_webp(&webp);
        cover.set_path(Some(&webp));
        pump_until(|| cover.picture.is_visible(), Duration::from_secs(5));
        assert!(cover.picture.is_visible(), "a WebP cover should decode via the image-crate fallback");
        assert!(!cover.placeholder.is_visible());

        cover.set_path(None);
        assert!(cover.placeholder.is_visible(), "clearing the path restores the placeholder");
    }

    /// The regression this fix guards against: decoding at logical `size` alone (ignoring the
    /// display's scale factor) produces a texture with fewer physical pixels than a HiDPI/
    /// scaled surface needs to fill without upscaling — which is what made every cover look
    /// blurry, uniformly, regardless of how small the slot was. A plain unit test on the pure
    /// function, not a GTK scenario, since Xvfb always reports scale factor 1 and can't exercise
    /// a scaled display at all.
    #[test]
    fn decode_pixels_scales_with_the_display_scale_factor() {
        assert_eq!(decode_pixels_for(64, 1), 64);
        assert_eq!(decode_pixels_for(64, 2), 128);
        assert_eq!(decode_pixels_for(48, 3), 144);
    }

    /// GTK never reports a scale factor below 1, but an unrealized widget's default shouldn't
    /// either — this is what keeps a not-yet-attached `CoverImage` from decoding at size 0.
    #[test]
    fn decode_pixels_never_scales_down_below_1x() {
        assert_eq!(decode_pixels_for(64, 0), 64);
    }

    /// The whole point of caching downscaled textures rather than native-resolution ones: the
    /// cached (and displayed) texture must always come out at exactly the requested size, never
    /// the source image's own resolution.
    pub(crate) fn run_decodes_at_the_requested_size_not_the_source_size(runtime: &tokio::runtime::Runtime) {
        let cover = CoverImage::new(64);
        let tmp = tempfile::tempdir().unwrap();
        let large = tmp.path().join("large.png");
        write_nxn_png(&large, 400);

        cover.set_path(Some(&large));
        pump_until(|| cover.picture.is_visible(), Duration::from_secs(5));

        let texture = cover.picture.paintable().and_then(|p| p.downcast::<gtk4::gdk::Texture>().ok()).expect("a texture should be showing");
        assert_eq!(texture.width(), 64);
        assert_eq!(texture.height(), 64);
        let _ = runtime;
    }

    /// A cache hit must never touch disk — decode a real file via one `CoverImage`, delete the
    /// file, then decode the *same* path+size via a second, independent instance. If the second
    /// instance re-read from disk it would fail (the file is gone) and fall back to the
    /// placeholder; showing the picture instead proves the cache served it.
    pub(crate) fn run_cache_hit_avoids_re_reading_disk(runtime: &tokio::runtime::Runtime) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cached.png");
        write_1x1_png(&path);

        let first = CoverImage::new(64);
        first.set_path(Some(&path));
        pump_until(|| first.picture.is_visible(), Duration::from_secs(5));

        std::fs::remove_file(&path).unwrap();

        let second = CoverImage::new(64);
        second.set_path(Some(&path));
        pump_until(|| second.picture.is_visible(), Duration::from_secs(5));
        assert!(second.picture.is_visible(), "a cache hit must show the picture even though the file is gone");
        let _ = runtime;
    }

    /// Low memory mode: a cover showing when the mode goes on turns into its placeholder, new
    /// covers aren't decoded, and leaving the mode brings them back.
    pub(crate) fn run_low_memory_mode_turns_covers_off_and_back_on(_runtime: &tokio::runtime::Runtime) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cover.png");
        write_1x1_png(&path);
        let showing = CoverImage::new(64);
        showing.set_path(Some(&path));
        pump_until(|| showing.picture.is_visible(), Duration::from_secs(5));

        set_low_memory_mode(true);
        assert!(!showing.picture.is_visible(), "a cover on screen becomes its placeholder at once");
        assert!(showing.placeholder.is_visible());
        let other_path = tmp.path().join("other.png");
        write_1x1_png(&other_path);
        let new_cover = CoverImage::new(64);
        new_cover.set_path(Some(&other_path));
        pump_until(|| false, Duration::from_millis(300));
        assert!(!new_cover.picture.is_visible(), "nothing is decoded in low memory mode");
        assert!(TEXTURE_CACHE.with(|cache| cache.borrow().used_bytes) <= LOW_MEMORY_TEXTURE_BUDGET_BYTES);

        set_low_memory_mode(false);
        pump_until(|| showing.picture.is_visible() && new_cover.picture.is_visible(), Duration::from_secs(5));
        assert!(showing.picture.is_visible() && new_cover.picture.is_visible(), "covers come back when the mode goes off");
    }

    /// A late-landing decode for a path this widget has since moved on from must never clobber
    /// whatever it was rebound to — the exact scenario `last_path`'s `RefCell` (checked, not
    /// just replaced) guards against.
    pub(crate) fn run_a_stale_decode_does_not_clobber_a_newer_path(_runtime: &tokio::runtime::Runtime) {
        let tmp = tempfile::tempdir().unwrap();
        let path_a = tmp.path().join("a.png");
        let path_b = tmp.path().join("b.png");
        write_1x1_png(&path_a);
        write_1x1_png(&path_b);

        let cover = CoverImage::new(64);
        cover.set_path(Some(&path_a));
        // Immediately move on to B before A's background decode can possibly have landed.
        cover.set_path(Some(&path_b));

        pump_until(|| cover.picture.is_visible(), Duration::from_secs(5));
        // Give A's decode every chance to land too, then confirm B is still what's showing.
        pump_until(|| false, Duration::from_millis(200));
        assert_eq!(cover.last_path.borrow().as_deref(), Some(path_b.as_path()));
        assert!(cover.picture.is_visible());
    }

    /// Filling the cache past its byte budget must evict the least-recently-*used* entry, not
    /// simply the oldest-inserted one — re-accessing the first entry before pushing the budget
    /// again should save it from eviction.
    pub(crate) fn run_lru_eviction_keeps_recently_accessed_entries(runtime: &tokio::runtime::Runtime) {
        TEXTURE_CACHE.with(|cache| *cache.borrow_mut() = LruTextureCache::new(3 * 64 * 64 * 4));

        let tmp = tempfile::tempdir().unwrap();
        let make = |name: &str| {
            let path = tmp.path().join(name);
            write_1x1_png(&path);
            path
        };
        let paths: Vec<_> = ["a.png", "b.png", "c.png"].iter().map(|name| make(name)).collect();

        for path in &paths {
            let cover = CoverImage::new(64);
            cover.set_path(Some(path));
            pump_until(|| cover.picture.is_visible(), Duration::from_secs(5));
        }
        // Budget holds exactly 3 entries' worth — re-access "a" so it's the most recently used.
        let touch = CoverImage::new(64);
        touch.set_path(Some(&paths[0]));
        pump_until(|| touch.picture.is_visible(), Duration::from_secs(5));

        // A fourth distinct entry must evict "b" (now the least-recently-used), not "a".
        let d_path = make("d.png");
        let filler = CoverImage::new(64);
        filler.set_path(Some(&d_path));
        pump_until(|| filler.picture.is_visible(), Duration::from_secs(5));

        std::fs::remove_file(&paths[0]).unwrap();
        let recheck_a = CoverImage::new(64);
        recheck_a.set_path(Some(&paths[0]));
        pump_until(|| recheck_a.picture.is_visible(), Duration::from_secs(5));
        assert!(recheck_a.picture.is_visible(), "\"a\" was recently touched, so it should still be cached even after the file is gone");
        let _ = runtime;
    }

    /// Not a `#[test]` itself — see `main.rs`'s `mod tests`. The cache counts hits, misses,
    /// evictions and re-decodes, and says so in one line.
    pub(crate) fn run_cache_counts_hits_misses_evictions_and_redecodes(runtime: &tokio::runtime::Runtime) {
        let texture = || -> gtk4::gdk::Texture {
            gtk4::gdk::MemoryTexture::new(64, 64, gtk4::gdk::MemoryFormat::R8g8b8a8, &glib::Bytes::from_owned(vec![0u8; 64 * 64 * 4]), 64 * 4).into()
        };
        let key = |name: &str| (PathBuf::from(name), 64);
        let mut cache = LruTextureCache::new(2 * 64 * 64 * 4);
        assert!(cache.get(&key("a")).is_none());
        cache.insert(key("a"), texture());
        cache.insert(key("b"), texture());
        cache.insert(key("c"), texture()); // evicts a
        cache.insert(key("a"), texture()); // evicts b; a had been held and thrown out: a re-decode
        assert!(cache.get(&key("c")).is_some());
        assert!(cache.get(&key("b")).is_none());
        assert_eq!(
            cache.stats,
            CacheStats { hits: 1, misses: 2, inserts: 4, evictions: 2, evicted_bytes: 2 * 64 * 64 * 4, redecodes: 1 }
        );
        let line = cache.report();
        for expected in ["entries=2", "used_kb=32", "budget_kb=32", "hits=1", "misses=2", "evictions=2", "redecodes=1", "entries_by_size=[64px:2]"] {
            assert!(line.contains(expected), "{expected} missing from {line}");
        }
        assert!(!line.contains("a.png") && !line.contains('/'), "no paths in the line: {line}");

        // What it remembers having evicted is bounded.
        let mut small = LruTextureCache::new(64 * 64 * 4);
        for index in 0..EVICTED_MEMORY + 50 {
            small.insert(key(&format!("cover-{index}")), texture());
        }
        assert_eq!(small.evicted.len(), EVICTED_MEMORY);
        assert!(live_cover_report().starts_with("live_covers="));
        let _ = runtime;
    }

    /// The smallest possible valid WebP (1x1, lossy VP8) — generated once with `cwebp`, so no
    /// WebP encoder is needed at test time.
    fn write_1x1_webp(path: &std::path::Path) {
        const WEBP_1X1: &[u8] = &[
            0x52, 0x49, 0x46, 0x46, 0x3c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x20, 0x30, 0x00, 0x00, 0x00, 0xd0,
            0x01, 0x00, 0x9d, 0x01, 0x2a, 0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x34, 0x25, 0xa0, 0x02, 0x74, 0xba, 0x01, 0xf8, 0x00, 0x03,
            0xb0, 0x00, 0xfe, 0xf0, 0xc4, 0x0b, 0xff, 0x20, 0xb9, 0x61, 0x75, 0xc8, 0xd7, 0xff, 0x20, 0x3f, 0xe4, 0x07, 0xfc, 0x80, 0xff,
            0xf8, 0xf2, 0x00, 0x00, 0x00,
        ];
        std::fs::write(path, WEBP_1X1).unwrap();
    }

    /// The smallest possible valid PNG (1x1, black pixel) — enough to decode without needing a
    /// real asset file in the repo.
    fn write_1x1_png(path: &std::path::Path) {
        const PNG_1X1: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63,
            0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
            0x42, 0x60, 0x82,
        ];
        std::fs::write(path, PNG_1X1).unwrap();
    }

    /// A real `n`x`n` PNG, built in-process via the `image` crate — used where the test needs a
    /// specific, larger-than-target source resolution (the 1x1 fixture above can't demonstrate a
    /// resize).
    fn write_nxn_png(path: &std::path::Path, n: u32) {
        let image = image::RgbaImage::from_pixel(n, n, image::Rgba([200, 100, 50, 255]));
        image.save(path).unwrap();
    }
}
