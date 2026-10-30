//! The full-screen cover viewer: tapping the cover in the Player or on a book's detail page opens
//! it over the whole window, in the app, at the server's full resolution, with pinch-to-zoom.
//!
//! Shown by swapping the window's content, like every other full page in this app (there is no
//! `AdwDialog` at this crate's libadwaita ceiling), and closing swaps the page it was opened from
//! back — the Player or detail page returns exactly as it was.
//!
//! The original can take seconds to arrive and decode on a Librem 5, so the page never waits for
//! it: it opens at once with the texture the tapped cover was already showing, swaps in the
//! uncropped cached copy (the server's 400 px resize) a moment later, and crossfades to the
//! original once it's decoded, with a pill at the bottom saying what it's doing meanwhile.
//! Low memory mode shows the cached copy only — an original costs tens of MB to decode.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::glib;
use adw::prelude::*;

use crate::i18n::{tr, tr_args, tr_noop};

/// The original is scaled down to this on its long side: a square cover is then at most ~26 MB
/// as a texture. Most covers are smaller anyway.
const ORIGINAL_MAX_SIDE: u32 = 2560;
/// The cached copy is the server's 400 px resize; this only guards against a surprise.
const THUMBNAIL_MAX_SIDE: u32 = 1024;
const MAX_ZOOM: f64 = 6.0;
/// With only the small copy there's no detail to find past this.
const LOW_MEMORY_MAX_ZOOM: f64 = 3.0;
const DOUBLE_TAP_ZOOM: f64 = 2.5;
const CROSSFADE_MS: u32 = 200;

pub(crate) const LOW_MEMORY_NOTE: &str = tr_noop("Low memory mode is on — showing a smaller copy");
pub(crate) const SMALLER_COPY_NOTE: &str = tr_noop("Showing a smaller copy — the full-size cover needs a connection");

/// What a viewer shows, and what it needs to fetch the original.
pub(crate) struct CoverSource {
    pub session: abs_core::auth::Session,
    pub paths: abs_storage::AppPaths,
    pub server_id: String,
    pub item_id: String,
    pub title: String,
    /// The cached (resized) cover file.
    pub thumbnail: PathBuf,
    /// The texture the tapped cover is showing, if any — shown the moment the page opens.
    pub shown: Option<gtk4::gdk::Texture>,
}

thread_local! {
    /// The open viewer, if any — one at a time.
    static CURRENT: RefCell<Option<CoverViewer>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Pill {
    Hidden,
    /// Something is on its way: a spinner and this text.
    Busy(String),
    /// A note that stays: no spinner.
    Note(String),
}

#[derive(Clone)]
pub(crate) struct CoverViewer {
    inner: Rc<Inner>,
}

struct Inner {
    window: gtk4::Window,
    previous: gtk4::Widget,
    root: gtk4::Overlay,
    scroller: gtk4::ScrolledWindow,
    stack: gtk4::Stack,
    pictures: [gtk4::Picture; 2],
    front: Cell<usize>,
    pill: gtk4::Box,
    spinner: gtk4::Spinner,
    pill_label: gtk4::Label,
    pill_state: RefCell<Pill>,
    zoom: Cell<f64>,
    max_zoom: f64,
    /// The shown image's size in pixels, for its aspect ratio.
    image_size: Cell<Option<(f64, f64)>>,
    showing_original: Cell<bool>,
    /// Set while the download's progress may still update the pill.
    downloading: Cell<bool>,
    closed: Cell<bool>,
}

/// Opens the viewer over the window `anchor` is in. `None` when it can't (no window) or one is
/// already open.
pub(crate) fn open(anchor: &gtk4::Widget, source: CoverSource) -> Option<CoverViewer> {
    let window = anchor.root()?.downcast::<gtk4::Window>().ok()?;
    if let Some(existing) = current() {
        if existing.is_shown() {
            return None;
        }
        // Left behind by something else swapping the window's content: forget it.
        existing.inner.closed.set(true);
        CURRENT.with(|c| c.borrow_mut().take());
    }
    let previous = host_content(&window)?;
    let low_memory = crate::widgets::cover_image::low_memory_mode();
    let viewer = CoverViewer::build(window, previous, if low_memory { LOW_MEMORY_MAX_ZOOM } else { MAX_ZOOM }, &source.title);
    set_host_content(&viewer.inner.window, viewer.inner.root.upcast_ref());
    CURRENT.with(|c| *c.borrow_mut() = Some(viewer.clone()));
    tracing::info!(item_id = %source.item_id, low_memory, "opened the cover viewer");
    viewer.load(source, low_memory);
    Some(viewer)
}

/// The open viewer, if any.
pub(crate) fn current() -> Option<CoverViewer> {
    CURRENT.with(|c| c.borrow().clone())
}

/// Closes the viewer if one is on screen; `true` if it did. The Player's Escape (`player.collapse`,
/// an app-wide accelerator) asks this first, so Escape closes the viewer rather than the Player
/// underneath it.
pub(crate) fn close_current() -> bool {
    match current() {
        Some(viewer) if viewer.is_shown() => {
            viewer.close();
            true
        }
        _ => false,
    }
}

impl CoverViewer {
    fn build(window: gtk4::Window, previous: gtk4::Widget, max_zoom: f64, title: &str) -> Self {
        ensure_css();
        let picture = || {
            gtk4::Picture::builder().content_fit(gtk4::ContentFit::Contain).hexpand(true).vexpand(true).can_shrink(true).build()
        };
        let pictures = [picture(), picture()];
        let stack = gtk4::Stack::builder().hexpand(true).vexpand(true).transition_duration(CROSSFADE_MS).build();
        stack.add_child(&pictures[0]);
        stack.add_child(&pictures[1]);
        let scroller = gtk4::ScrolledWindow::builder().hexpand(true).vexpand(true).child(&stack).build();

        let close_button = gtk4::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text(tr("Close"))
            .css_classes(["circular", "osd"])
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .margin_top(12)
            .margin_start(12)
            .build();
        let title_label = gtk4::Label::builder()
            .label(title)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .css_classes(["cover-viewer-title"])
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Start)
            .margin_top(20)
            .margin_start(64)
            .margin_end(64)
            .can_target(false)
            .build();
        let spinner = gtk4::Spinner::new();
        let pill_label = gtk4::Label::builder().wrap(true).wrap_mode(gtk4::pango::WrapMode::WordChar).justify(gtk4::Justification::Center).build();
        let pill = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["osd", "cover-viewer-pill"])
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::End)
            .margin_bottom(24)
            .margin_start(16)
            .margin_end(16)
            .can_target(false)
            .visible(false)
            .build();
        pill.append(&spinner);
        pill.append(&pill_label);

        let root = gtk4::Overlay::builder().css_classes(["cover-viewer"]).child(&scroller).build();
        root.add_overlay(&title_label);
        root.add_overlay(&pill);
        root.add_overlay(&close_button);

        let viewer = Self {
            inner: Rc::new(Inner {
                window,
                previous,
                root,
                scroller,
                stack,
                pictures,
                front: Cell::new(1),
                pill,
                spinner,
                pill_label,
                pill_state: RefCell::new(Pill::Hidden),
                zoom: Cell::new(1.0),
                max_zoom,
                image_size: Cell::new(None),
                showing_original: Cell::new(false),
                downloading: Cell::new(false),
                closed: Cell::new(false),
            }),
        };
        close_button.connect_clicked({
            let viewer = viewer.downgrade();
            move |_| {
                if let Some(inner) = viewer.upgrade() {
                    CoverViewer { inner }.close();
                }
            }
        });
        // Focus inside the page, so Escape reaches its key controller.
        viewer.inner.root.connect_map({
            let close_button = close_button.clone();
            move |_| {
                close_button.grab_focus();
            }
        });
        viewer.add_controllers();
        viewer
    }

    fn downgrade(&self) -> std::rc::Weak<Inner> {
        Rc::downgrade(&self.inner)
    }

    fn add_controllers(&self) {
        let inner = &self.inner;

        let escape = gtk4::EventControllerKey::new();
        escape.set_propagation_phase(gtk4::PropagationPhase::Capture);
        escape.connect_key_pressed({
            let viewer = self.downgrade();
            move |_, key, _, _| {
                if key != gtk4::gdk::Key::Escape {
                    return glib::Propagation::Proceed;
                }
                if let Some(inner) = viewer.upgrade() {
                    CoverViewer { inner }.close();
                }
                glib::Propagation::Stop
            }
        });
        inner.root.add_controller(escape);

        // Swipe down to close, as the Player itself collapses — only while not zoomed, when a
        // drag isn't panning the image.
        let swipe = gtk4::GestureDrag::new();
        swipe.connect_drag_end({
            let viewer = self.downgrade();
            move |gesture, _, _| {
                let Some(inner) = viewer.upgrade() else { return };
                let viewer = CoverViewer { inner };
                if viewer.zoom() > 1.0 + f64::EPSILON {
                    return;
                }
                if let Some((x, y)) = gesture.offset() {
                    if crate::screens::player::player_gesture_should_collapse(x, y) {
                        viewer.close();
                    }
                }
            }
        });
        inner.root.add_controller(swipe);

        // Pinch: ahead of the scroller's own touch handling, and claiming both fingers so the
        // scroller doesn't pan at the same time.
        let pinch = gtk4::GestureZoom::new();
        pinch.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let pinch_start = Rc::new(Cell::new(1.0));
        pinch.connect_begin({
            let viewer = self.downgrade();
            let pinch_start = pinch_start.clone();
            move |gesture, _| {
                if let Some(inner) = viewer.upgrade() {
                    pinch_start.set(inner.zoom.get());
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                }
            }
        });
        pinch.connect_scale_changed({
            let viewer = self.downgrade();
            move |gesture, scale| {
                let Some(inner) = viewer.upgrade() else { return };
                let viewer = CoverViewer { inner };
                let focus = gesture.bounding_box_center().unwrap_or_else(|| viewer.viewport_center());
                viewer.zoom_to(pinch_start.get() * scale, focus);
            }
        });
        inner.scroller.add_controller(pinch);

        let double_tap = gtk4::GestureClick::new();
        double_tap.connect_pressed({
            let viewer = self.downgrade();
            move |_, n_press, x, y| {
                if n_press != 2 {
                    return;
                }
                let Some(inner) = viewer.upgrade() else { return };
                let viewer = CoverViewer { inner };
                let target = if viewer.zoom() > 1.0 + f64::EPSILON { 1.0 } else { DOUBLE_TAP_ZOOM };
                viewer.zoom_to(target, (x, y));
            }
        });
        inner.scroller.add_controller(double_tap);

        // Ctrl+scroll zooms around the pointer (desktop).
        let pointer = Rc::new(Cell::new(None::<(f64, f64)>));
        let motion = gtk4::EventControllerMotion::new();
        motion.connect_motion({
            let pointer = pointer.clone();
            move |_, x, y| pointer.set(Some((x, y)))
        });
        inner.scroller.add_controller(motion);
        let wheel = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk4::PropagationPhase::Capture);
        wheel.connect_scroll({
            let viewer = self.downgrade();
            move |controller, _, dy| {
                if !controller.current_event_state().contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                    return glib::Propagation::Proceed;
                }
                let Some(inner) = viewer.upgrade() else { return glib::Propagation::Stop };
                let viewer = CoverViewer { inner };
                let focus = pointer.get().unwrap_or_else(|| viewer.viewport_center());
                viewer.zoom_to(viewer.zoom() * 1.15_f64.powf(-dy), focus);
                glib::Propagation::Stop
            }
        });
        inner.scroller.add_controller(wheel);
    }

    /// Shows what's at hand at once, then the uncropped cached copy, then (unless in low memory
    /// mode) the original.
    fn load(&self, source: CoverSource, low_memory: bool) {
        if let Some(texture) = &source.shown {
            self.show(texture, false);
        }

        let viewer = self.clone();
        let thumbnail = source.thumbnail.clone();
        glib::spawn_future_local(async move {
            let Some(texture) = decode_off_thread(thumbnail, THUMBNAIL_MAX_SIDE).await else { return };
            if !viewer.inner.closed.get() && !viewer.inner.showing_original.get() {
                viewer.show(&texture, false);
            }
        });

        if low_memory {
            self.set_pill(Pill::Note(tr(LOW_MEMORY_NOTE)));
            return;
        }

        let viewer = self.clone();
        glib::spawn_future_local(async move {
            let original = viewer.original(&source).await;
            if viewer.inner.closed.get() {
                return;
            }
            let Some(path) = original else {
                viewer.set_pill(Pill::Note(tr(SMALLER_COPY_NOTE)));
                return;
            };
            viewer.set_pill(Pill::Busy(tr("Preparing full size…")));
            let started = std::time::Instant::now();
            let decoded = decode_off_thread(path, ORIGINAL_MAX_SIDE).await;
            if viewer.inner.closed.get() {
                return;
            }
            match decoded {
                Some(texture) => {
                    tracing::info!(
                        item_id = %source.item_id,
                        width = texture.width(),
                        height = texture.height(),
                        decode_ms = started.elapsed().as_millis() as u64,
                        "showing the full-size cover"
                    );
                    viewer.inner.showing_original.set(true);
                    viewer.show(&texture, true);
                    viewer.set_pill(Pill::Hidden);
                }
                None => viewer.set_pill(Pill::Note(tr(SMALLER_COPY_NOTE))),
            }
        });
    }

    /// The original's file: from the cache, or downloaded (its progress shown in the pill). The
    /// download runs on its own task, so closing the viewer doesn't stop it — the next open
    /// finds it cached.
    async fn original(&self, source: &CoverSource) -> Option<PathBuf> {
        if let Some(cached) = abs_core::covers::cached_original_cover(&source.paths, &source.server_id, &source.item_id).await {
            return Some(cached);
        }
        if source.session.is_offline() {
            return None;
        }
        self.inner.downloading.set(true);
        self.set_pill(Pill::Busy(downloading_text(0, None)));
        let (progress, mut progress_rx) = tokio::sync::watch::channel((0_u64, None::<u64>));
        let fetch = tokio::spawn({
            let session = source.session.clone();
            let paths = source.paths.clone();
            let server_id = source.server_id.clone();
            let item_id = source.item_id.clone();
            async move {
                let connection = session.connection_target().await.ok()?;
                let access_token = session.access_token().await;
                abs_core::covers::fetch_original_cover(&paths, &connection, &access_token, &server_id, &item_id, move |received, total| {
                    let _ = progress.send((received, total));
                })
                .await
            }
        });
        let viewer = self.clone();
        glib::spawn_future_local(async move {
            while progress_rx.changed().await.is_ok() {
                if viewer.inner.closed.get() || !viewer.inner.downloading.get() {
                    return;
                }
                let (received, total) = *progress_rx.borrow();
                viewer.set_pill(Pill::Busy(downloading_text(received, total)));
            }
        });
        let path = fetch.await.ok().flatten();
        self.inner.downloading.set(false);
        path
    }

    fn show(&self, texture: &gtk4::gdk::Texture, crossfade: bool) {
        let inner = &self.inner;
        let next = 1 - inner.front.get();
        inner.pictures[next].set_paintable(Some(texture));
        inner.stack.set_transition_type(if crossfade { gtk4::StackTransitionType::Crossfade } else { gtk4::StackTransitionType::None });
        inner.stack.set_visible_child(&inner.pictures[next]);
        inner.front.set(next);
        inner.image_size.set(Some((texture.width() as f64, texture.height() as f64)));
        // The texture behind it is released once the crossfade is over.
        let old = inner.pictures[1 - next].clone();
        let stack = inner.stack.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(u64::from(CROSSFADE_MS) + 50), move || {
            if stack.visible_child().as_ref() != Some(old.upcast_ref()) {
                old.set_paintable(None::<&gtk4::gdk::Paintable>);
            }
        });
        self.update_content_size();
    }

    fn set_pill(&self, pill: Pill) {
        let inner = &self.inner;
        match &pill {
            Pill::Hidden => inner.pill.set_visible(false),
            Pill::Busy(text) | Pill::Note(text) => {
                inner.pill_label.set_label(text);
                let busy = matches!(pill, Pill::Busy(_));
                inner.spinner.set_visible(busy);
                inner.spinner.set_spinning(busy);
                inner.pill.set_visible(true);
            }
        }
        *inner.pill_state.borrow_mut() = pill;
    }

    fn zoom(&self) -> f64 {
        self.inner.zoom.get()
    }

    fn viewport(&self) -> (f64, f64) {
        (self.inner.scroller.width() as f64, self.inner.scroller.height() as f64)
    }

    fn viewport_center(&self) -> (f64, f64) {
        let (width, height) = self.viewport();
        (width / 2.0, height / 2.0)
    }

    /// Zooms to `zoom` (clamped), keeping the image point under `focus` (viewport coordinates)
    /// where it is.
    fn zoom_to(&self, zoom: f64, focus: (f64, f64)) {
        let inner = &self.inner;
        let old = inner.zoom.get();
        let new = clamp_zoom(zoom, inner.max_zoom);
        let (Some(image), (vw, vh)) = (inner.image_size.get(), self.viewport()) else { return };
        if vw <= 0.0 || vh <= 0.0 || (new - old).abs() < f64::EPSILON {
            return;
        }
        let (fw, fh) = fit_size(image, (vw, vh));
        let (hadj, vadj) = (inner.scroller.hadjustment(), inner.scroller.vadjustment());
        let x = anchored_scroll(fw, vw, old, new, focus.0, hadj.value());
        let y = anchored_scroll(fh, vh, old, new, focus.1, vadj.value());
        inner.zoom.set(new);
        self.update_content_size();
        // The new extent is set now rather than at the next layout, so the scroll position
        // can be set with it and the image doesn't jump for a frame.
        hadj.set_upper((fw * new).max(vw));
        vadj.set_upper((fh * new).max(vh));
        hadj.set_value(x);
        vadj.set_value(y);
    }

    fn update_content_size(&self) {
        let inner = &self.inner;
        let zoom = inner.zoom.get();
        let (vw, vh) = self.viewport();
        match inner.image_size.get() {
            Some(image) if zoom > 1.0 + f64::EPSILON && vw > 0.0 && vh > 0.0 => {
                let (fw, fh) = fit_size(image, (vw, vh));
                inner.stack.set_size_request((fw * zoom).round() as i32, (fh * zoom).round() as i32);
            }
            // At 1× the image just fills the page, whatever its size.
            _ => inner.stack.set_size_request(-1, -1),
        }
    }

    fn is_shown(&self) -> bool {
        host_content(&self.inner.window).as_ref() == Some(self.inner.root.upcast_ref())
    }

    /// Brings back the page it was opened from and lets go of the images.
    pub(crate) fn close(&self) {
        let inner = &self.inner;
        if inner.closed.replace(true) {
            return;
        }
        for picture in &inner.pictures {
            picture.set_paintable(None::<&gtk4::gdk::Paintable>);
        }
        set_host_content(&inner.window, &inner.previous);
        CURRENT.with(|c| {
            let mut current = c.borrow_mut();
            if current.as_ref().is_some_and(|open| Rc::ptr_eq(&open.inner, inner)) {
                current.take();
            }
        });
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> &gtk4::Overlay {
        &self.inner.root
    }

    /// The texture on screen (test view).
    #[cfg(test)]
    pub(crate) fn shown_texture(&self) -> Option<gtk4::gdk::Texture> {
        let inner = &self.inner;
        inner.pictures[inner.front.get()].paintable().and_then(|p| p.downcast::<gtk4::gdk::Texture>().ok())
    }

    #[cfg(test)]
    pub(crate) fn pill(&self) -> Pill {
        self.inner.pill_state.borrow().clone()
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.closed.get()
    }

    #[cfg(test)]
    pub(crate) fn zoom_for_test(&self, zoom: f64) {
        self.zoom_to(zoom, self.viewport_center());
    }
}

fn host_content(window: &gtk4::Window) -> Option<gtk4::Widget> {
    if let Some(window) = window.downcast_ref::<adw::ApplicationWindow>() {
        return window.content();
    }
    if let Some(window) = window.downcast_ref::<adw::Window>() {
        return window.content();
    }
    window.child()
}

/// Deferred to idle like `widgets::swap_content`: a page swap from inside one of the page's own
/// event handlers would otherwise unparent the widget mid-event.
fn set_host_content(window: &gtk4::Window, content: &gtk4::Widget) {
    let window = window.clone();
    let content = content.clone();
    glib::idle_add_local_once(move || {
        if let Some(window) = window.downcast_ref::<adw::ApplicationWindow>() {
            window.set_content(Some(&content));
        } else if let Some(window) = window.downcast_ref::<adw::Window>() {
            window.set_content(Some(&content));
        } else {
            window.set_child(Some(&content));
        }
    });
}

/// Decodes `path` on a blocking thread, scaled down to `max_side` on its long side if bigger,
/// aspect kept; the texture itself is built back on the main thread (see
/// `cover_image::decode_and_crop_to_cover` for why). `None` (logged) if it can't be read.
async fn decode_off_thread(path: PathBuf, max_side: u32) -> Option<gtk4::gdk::Texture> {
    let decoded = tokio::task::spawn_blocking({
        let path = path.clone();
        move || decode_fitted(&path, max_side)
    })
    .await;
    let (bytes, width, height) = match decoded {
        Ok(Ok(decoded)) => decoded,
        Ok(Err(err)) => {
            tracing::warn!(%err, ?path, "couldn't decode the cover for the viewer");
            return None;
        }
        Err(err) => {
            tracing::warn!(%err, ?path, "the cover decode for the viewer panicked");
            return None;
        }
    };
    Some(
        gtk4::gdk::MemoryTexture::new(
            width as i32,
            height as i32,
            gtk4::gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(bytes),
            (width * 4) as usize,
        )
        .upcast(),
    )
}

fn decode_fitted(path: &std::path::Path, max_side: u32) -> Result<(Vec<u8>, u32, u32), image::ImageError> {
    let bytes = std::fs::read(path)?;
    let mut decoded = image::load_from_memory(&bytes)?;
    drop(bytes);
    let (width, height) = (decoded.width(), decoded.height());
    if width.max(height) > max_side {
        decoded = decoded.resize(max_side, max_side, image::imageops::FilterType::Triangle);
    }
    let rgba = decoded.into_rgba8();
    let (width, height) = rgba.dimensions();
    Ok((rgba.into_raw(), width, height))
}

fn ensure_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_data(
            "overlay.cover-viewer { background-color: black; } \
             overlay.cover-viewer label.cover-viewer-title { color: white; font-weight: 700; text-shadow: 0 1px 3px black; } \
             box.cover-viewer-pill { border-radius: 999px; padding: 8px 16px; }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("a display for the app's css"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

fn clamp_zoom(zoom: f64, max_zoom: f64) -> f64 {
    if zoom.is_finite() { zoom.clamp(1.0, max_zoom) } else { 1.0 }
}

/// The size an `image` (w, h) is shown at when fitted, whole, into `viewport` (w, h).
fn fit_size(image: (f64, f64), viewport: (f64, f64)) -> (f64, f64) {
    if image.0 <= 0.0 || image.1 <= 0.0 {
        return viewport;
    }
    let scale = (viewport.0 / image.0).min(viewport.1 / image.1);
    (image.0 * scale, image.1 * scale)
}

/// One axis of a zoom around a focus point: the scroll position after going from `old_zoom` to
/// `new_zoom` that keeps the image point under `focus` (viewport coordinates) under it. `fit` is
/// the image's fitted size on this axis, `viewport` the page's. Content smaller than the page is
/// centered in it (the picture is allocated the whole page and draws the image in its middle),
/// which is accounted for on both sides.
fn anchored_scroll(fit: f64, viewport: f64, old_zoom: f64, new_zoom: f64, focus: f64, scroll: f64) -> f64 {
    let offset = |zoom: f64| ((fit * zoom).max(viewport) - fit * zoom) / 2.0;
    let point = (scroll + focus - offset(old_zoom)) / old_zoom;
    let extent = (fit * new_zoom).max(viewport);
    (point * new_zoom + offset(new_zoom) - focus).clamp(0.0, extent - viewport)
}

/// The pill's text while the original downloads.
fn downloading_text(received: u64, total: Option<u64>) -> String {
    match total {
        // TRANSLATORS: {percent} is a whole number from 0 to 100; the "%" sign is part of this text.
        Some(total) if total > 0 => tr_args("Downloading full size… {percent} %", &[("percent", &((received.min(total) * 100) / total).to_string())]),
        _ => tr("Downloading full size…"),
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn zoom_is_kept_between_fit_and_the_maximum() {
        assert_eq!(clamp_zoom(0.5, MAX_ZOOM), 1.0);
        assert_eq!(clamp_zoom(2.0, MAX_ZOOM), 2.0);
        assert_eq!(clamp_zoom(10.0, MAX_ZOOM), MAX_ZOOM);
        assert_eq!(clamp_zoom(10.0, LOW_MEMORY_MAX_ZOOM), LOW_MEMORY_MAX_ZOOM);
        assert_eq!(clamp_zoom(f64::NAN, MAX_ZOOM), 1.0);
    }

    #[test]
    fn an_image_is_fitted_whole_with_its_aspect_kept() {
        assert_eq!(fit_size((1000.0, 1500.0), (360.0, 600.0)), (360.0, 540.0));
        assert_eq!(fit_size((400.0, 400.0), (360.0, 600.0)), (360.0, 360.0), "small copies are scaled up to fit");
    }

    #[test]
    fn zooming_keeps_the_point_under_the_fingers() {
        // A 360 px wide page with the image fitted to 360 px: zooming 1× → 3× around x = 90.
        let scroll = anchored_scroll(360.0, 360.0, 1.0, 3.0, 90.0, 0.0);
        // The image point under x = 90 was 90 (of 360); at 3× it's at 270, which must be at 90.
        assert_eq!(scroll, 180.0);
        // And back, from there.
        assert_eq!(anchored_scroll(360.0, 360.0, 3.0, 1.0, 90.0, 180.0), 0.0);
        // 2× → 4× around the middle of a scrolled view.
        let point = (100.0 + 180.0) / 2.0;
        let scroll = anchored_scroll(360.0, 360.0, 2.0, 4.0, 180.0, 100.0);
        assert_eq!(scroll, point * 4.0 - 180.0);
    }

    #[test]
    fn zooming_accounts_for_an_image_centered_in_a_taller_page() {
        // 360×360 image in a 360×600 page: centered vertically with 120 px above it. Zooming
        // around y = 300 (the image's middle) to 2×: the middle stays at 300.
        let scroll = anchored_scroll(360.0, 600.0, 1.0, 2.0, 300.0, 0.0);
        // 2× is 720 tall: the middle (360) at 300 means scrolled by 60.
        assert_eq!(scroll, 60.0);
    }

    #[test]
    fn zooming_never_scrolls_past_the_edges() {
        assert_eq!(anchored_scroll(360.0, 360.0, 1.0, 2.0, 0.0, 0.0), 0.0);
        assert_eq!(anchored_scroll(360.0, 360.0, 1.0, 2.0, 360.0, 0.0), 360.0);
    }

    #[test]
    fn the_download_shows_a_percentage_when_the_size_is_known() {
        assert_eq!(downloading_text(0, None), "Downloading full size…");
        assert_eq!(downloading_text(450, Some(1000)), "Downloading full size… 45 %");
        assert_eq!(downloading_text(1000, Some(1000)), "Downloading full size… 100 %");
        assert_eq!(downloading_text(10, Some(0)), "Downloading full size…");
    }

    #[test]
    fn a_big_original_is_scaled_down_with_its_aspect_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.png");
        image::RgbImage::new(3000, 1500).save(&path).unwrap();
        let (bytes, width, height) = decode_fitted(&path, ORIGINAL_MAX_SIDE).unwrap();
        assert_eq!((width, height), (2560, 1280));
        assert_eq!(bytes.len(), 2560 * 1280 * 4);
        let small = tmp.path().join("small.png");
        image::RgbImage::new(300, 450).save(&small).unwrap();
        assert_eq!(decode_fitted(&small, ORIGINAL_MAX_SIDE).unwrap().1, 300, "never scaled up");
    }
}

/// Shared setup for the viewer's scenarios (in `screens::player` and `screens::item_detail`, next
/// to the screens they tap on).
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::pump_until;
    use std::time::Duration;

    /// Cover-like art with fine detail (thin stripes and a checkerboard), so a zoomed-in
    /// screenshot shows whether the full-size original or the small copy is on screen.
    pub(crate) fn cover_art(width: u32, height: u32) -> image::RgbImage {
        image::RgbImage::from_fn(width, height, |x, y| {
            let (fx, fy) = (x as f32 / width as f32, y as f32 / height as f32);
            if (0.30..0.42).contains(&fy) && (0.1..0.9).contains(&fx) {
                // A "title" block of one-pixel-per-step stripes: crisp only at full size.
                let stripe = (x * 1600 / width) % 6 < 3 && (y * 2400 / height) % 6 < 3;
                return if stripe { image::Rgb([250, 240, 220]) } else { image::Rgb([40, 30, 60]) };
            }
            if (0.75..0.85).contains(&fy) && (0.2..0.8).contains(&fx) {
                let check = ((x * 1600 / width) / 12 + (y * 2400 / height) / 12).is_multiple_of(2);
                return if check { image::Rgb([230, 200, 40]) } else { image::Rgb([30, 30, 30]) };
            }
            image::Rgb([(40.0 + 120.0 * fy) as u8, (60.0 + 40.0 * fx) as u8, (120.0 + 80.0 * (1.0 - fy)) as u8])
        })
    }

    pub(crate) fn png(image: &image::RgbImage) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    pub(crate) const ORIGINAL_SIZE: (u32, u32) = (1200, 1800);
    /// The server's resize of it: 400 px wide.
    pub(crate) const THUMBNAIL_SIZE: (u32, u32) = (400, 600);

    /// Serves the original at `?raw=1`, after `delay`, `expect` times.
    pub(crate) async fn mount_original(mock_server: &wiremock::MockServer, item_id: &str, delay: Duration, expect: u64) {
        let original = png(&cover_art(ORIGINAL_SIZE.0, ORIGINAL_SIZE.1));
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/api/items/{item_id}/cover")))
            .and(wiremock::matchers::query_param("raw", "1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(original, "image/png").set_delay(delay))
            .expect(expect)
            .mount(mock_server)
            .await;
    }

    /// The cached (resized) cover, as the covers cache would have stored it.
    pub(crate) async fn seed_thumbnail(pool: &sqlx::SqlitePool, paths: &abs_storage::AppPaths, server_id: &str, item_id: &str) -> PathBuf {
        let path = paths.cover_cache_path(server_id, item_id, "png");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let thumbnail = image::imageops::resize(&cover_art(ORIGINAL_SIZE.0, ORIGINAL_SIZE.1), THUMBNAIL_SIZE.0, THUMBNAIL_SIZE.1, image::imageops::FilterType::Triangle);
        std::fs::write(&path, png(&thumbnail)).unwrap();
        abs_storage::repo::items::set_cover_cache_path(pool, server_id, item_id, Some(&path.to_string_lossy())).await.unwrap();
        path
    }

    pub(crate) fn texture_size(viewer: &CoverViewer) -> Option<(i32, i32)> {
        viewer.shown_texture().map(|t| (t.width(), t.height()))
    }

    pub(crate) fn wait_for_texture(viewer: &CoverViewer, size: (u32, u32), timeout: Duration) {
        let wanted = Some((size.0 as i32, size.1 as i32));
        pump_until(|| texture_size(viewer) == wanted, timeout);
        assert_eq!(texture_size(viewer), wanted, "the viewer should be showing a {size:?} image");
    }

    /// Waits for the viewer opened by a tap to be the window's page.
    pub(crate) fn wait_until_shown(timeout: Duration) -> CoverViewer {
        pump_until(|| current().is_some_and(|v| v.is_shown()), timeout);
        current().filter(|v| v.is_shown()).expect("the cover viewer should be on screen")
    }

    pub(crate) fn content_of(window: &gtk4::Window) -> Option<gtk4::Widget> {
        host_content(window)
    }

    pub(crate) fn zoom_of(viewer: &CoverViewer) -> f64 {
        viewer.zoom()
    }

    pub(crate) fn close_button(viewer: &CoverViewer) -> gtk4::Button {
        crate::widgets::find_descendant::<gtk4::Button>(viewer.root().upcast_ref()).expect("the viewer has a close button")
    }

    pub(crate) fn scroller(viewer: &CoverViewer) -> gtk4::ScrolledWindow {
        viewer.inner.scroller.clone()
    }
}
