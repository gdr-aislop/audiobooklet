//! A small GStreamer-backed audio playback engine behind the [`AudioBackend`] trait, so
//! `abs-core`'s playback state machine (`abs_core::playback::PlaybackState`) can be driven
//! without depending on GStreamer directly. This crate is the one place that touches real audio
//! I/O — everything else in the workspace (including `abs-core`) stays testable without a sound
//! device or a GLib main loop.

pub mod call_watch;
pub mod connectivity_watch;
mod error;
pub mod mpris;
pub mod network_watch;
pub mod route_watch;

use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

pub use error::{PlaybackError, PlaybackErrorKind, PlayerError, Result};

/// Call once per process before constructing a [`GstBackend`]. Safe to call more than once
/// (`gstreamer::init` is idempotent).
pub fn init() -> Result<()> {
    gst::init()?;
    Ok(())
}

/// Whether GStreamer has finished initialising in this process (by [`init`] or
/// [`init_in_background`]); `gst_is_initialized` turns true only at the end of the init.
pub fn is_initialized() -> bool {
    unsafe { gst::ffi::gst_is_initialized() != 0 }
}

/// GStreamer initialising on its own thread (see [`init_in_background`]).
///
/// `gst::init` loads the plugin registry and, when the cache is stale, scans every plugin: on the
/// phone that has taken seconds, and nothing needs it until the player is built. Starting it
/// before the database opens lets the two overlap. Cloning is cheap; every clone sees the same
/// outcome.
#[derive(Clone)]
pub struct GstInit {
    outcome: futures::future::Shared<futures::channel::oneshot::Receiver<std::result::Result<(), String>>>,
    finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl GstInit {
    /// Resolves once initialisation has finished: `Err` carries the failure's text. Never blocks
    /// the thread polling it, so a GLib main-loop future can await it.
    pub async fn wait(&self) -> std::result::Result<(), String> {
        match self.outcome.clone().await {
            Ok(outcome) => outcome,
            Err(_) => Err("the GStreamer initialisation thread ended without a result".to_string()),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Starts `gst::init` on a thread named `abs-gst-init` and returns at once. `init` itself stays
/// the synchronous way (tests, tools). `took` is called on that thread with how long it took.
pub fn init_in_background(took: impl FnOnce(Duration, &std::result::Result<(), String>) + Send + 'static) -> GstInit {
    init_in_background_with(|| init().map_err(|err| err.to_string()), took)
}

#[doc(hidden)]
pub fn init_in_background_with(
    work: impl FnOnce() -> std::result::Result<(), String> + Send + 'static,
    took: impl FnOnce(Duration, &std::result::Result<(), String>) + Send + 'static,
) -> GstInit {
    let (sender, receiver) = futures::channel::oneshot::channel();
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished_for_thread = finished.clone();
    let spawned = std::thread::Builder::new().name("abs-gst-init".into()).spawn(move || {
        let started = std::time::Instant::now();
        let outcome = work();
        took(started.elapsed(), &outcome);
        finished_for_thread.store(true, std::sync::atomic::Ordering::Release);
        let _ = sender.send(outcome);
    });
    if let Err(err) = spawned {
        // No thread to run on: the sender was dropped with the closure, so waiters see the failure.
        tracing::warn!(%err, "couldn't start the GStreamer initialisation thread");
    }
    GstInit { outcome: futures::FutureExt::shared(receiver), finished }
}

/// A bus event a caller should react to: end of stream (advance to the next chapter, or stop)
/// and pipeline errors (surface to the user, stop playback). Polled rather than delivered via
/// callback so this stays simple to drive from a GLib main-loop idle source or a plain timer,
/// whichever the `app` crate's event loop integration prefers.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    EndOfStream,
    Error(PlaybackError),
}

/// Classifies a GStreamer bus error into a [`PlaybackErrorKind`] a UI can pick a friendly message
/// from — `glib::Error`'s own domain/code is checked against each of GStreamer's error enums in
/// turn (a message only ever matches one domain, so order between them doesn't matter beyond
/// putting the more specific "this file's format" cases ahead of the generic resource-failure
/// catch-all). Falls back to [`PlaybackErrorKind::Other`] for anything not covered — this
/// classification is deliberately a small, UI-relevant taxonomy, not an exhaustive mirror of
/// GStreamer's own.
fn classify_gst_error(err: &gst::glib::Error) -> PlaybackErrorKind {
    if let Some(kind) = err.kind::<gst::CoreError>() {
        if kind == gst::CoreError::MissingPlugin {
            return PlaybackErrorKind::MissingCodec;
        }
    }
    if let Some(kind) = err.kind::<gst::StreamError>() {
        return match kind {
            gst::StreamError::CodecNotFound => PlaybackErrorKind::MissingCodec,
            gst::StreamError::Decode | gst::StreamError::WrongType | gst::StreamError::TypeNotFound | gst::StreamError::Demux | gst::StreamError::Mux => {
                PlaybackErrorKind::UnsupportedOrCorrupt
            }
            _ => PlaybackErrorKind::Other,
        };
    }
    if let Some(kind) = err.kind::<gst::ResourceError>() {
        return match kind {
            gst::ResourceError::NotFound | gst::ResourceError::OpenRead => PlaybackErrorKind::ResourceNotFound,
            gst::ResourceError::NotAuthorized => PlaybackErrorKind::NotAuthorized,
            gst::ResourceError::OpenWrite | gst::ResourceError::Busy => PlaybackErrorKind::AudioOutput,
            gst::ResourceError::Read | gst::ResourceError::Sync | gst::ResourceError::Settings | gst::ResourceError::Failed => PlaybackErrorKind::Network,
            _ => PlaybackErrorKind::Other,
        };
    }
    PlaybackErrorKind::Other
}

/// [`classify_gst_error`] plus what the bus message's debug string adds: a generic stream failure
/// ("Internal data stream error", `StreamError::Failed`) that comes from the HTTP source is a
/// dropped or dead connection, not an unclassifiable fault.
fn classify_bus_error(err: &gst::glib::Error, debug: Option<&str>) -> PlaybackErrorKind {
    let kind = classify_gst_error(err);
    if kind == PlaybackErrorKind::Other && debug.is_some_and(|d| d.contains("GstSoupHTTPSrc") || d.contains("GstCurlHttpSrc")) {
        return PlaybackErrorKind::Network;
    }
    kind
}

/// Replaces the value of every `token=` query parameter in `text` with `REDACTED` — stream URLs
/// authenticate with `?token=<access token>`, and GStreamer quotes the URL in its error messages.
pub fn redact_tokens(text: &str) -> String {
    const KEY: &str = "token=";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(KEY) {
        let value_start = at + KEY.len();
        out.push_str(&rest[..value_start]);
        let value = &rest[value_start..];
        let value_len = value.find(|c: char| c == '&' || c == '#' || c == '"' || c == '\'' || c == ')' || c == '>' || c.is_whitespace()).unwrap_or(value.len());
        if value_len > 0 {
            out.push_str("REDACTED");
        }
        rest = &value[value_len..];
    }
    out.push_str(rest);
    out
}

/// Transport properties for HTTP(S) playback URIs — the playback-side mirror of the Connection
/// page's Advanced settings (custom headers, user agent, TLS verification). Plain data, no
/// reqwest: `abs-player` never touches the HTTP stack, it only hands these to GStreamer's HTTP
/// source (`souphttpsrc`) when the pipeline's source is being set up. Defaults describe the
/// behavior before connection settings existed: no extra headers, no override, verification on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectionProperties {
    /// Headers attached to every playback request (e.g. a reverse proxy's auth header).
    pub extra_headers: Vec<(String, String)>,
    /// Replaces souphttpsrc's default `User-Agent` when set.
    pub user_agent: Option<String>,
    /// `true` (the default) enforces certificate validation; the Connection page's
    /// "Disable SSL verification" setting maps to `false`.
    pub ssl_strict: bool,
}

/// The playback engine's public surface — implemented by [`GstBackend`] for real playback, and
/// mockable in `abs-core`'s own tests (as a trait object or a hand-rolled fake) without linking
/// GStreamer at all.
pub trait AudioBackend {
    fn load(&mut self, uri: &str) -> Result<()>;
    /// Sets the connection properties used by the **next** `load()` of an HTTP(S) URI. Local
    /// files are unaffected. Called before every streaming load so a settings change is picked
    /// up by the next track without rebuilding the backend.
    fn apply_connection(&mut self, properties: &ConnectionProperties);
    fn play(&mut self) -> Result<()>;
    /// Asks the pipeline to pause. `Ok` here means GStreamer accepted the request, **not** that
    /// the pipeline has actually reached `Paused` yet — `gst_element_set_state` returns `Ok` for
    /// an in-progress (`Async`) transition exactly as it does for one already complete; only a
    /// synchronous `Failure` is an `Err`. A network-streamed source mid-buffer-read can sit in
    /// `Async` for a long time, or effectively forever, entirely silently — see [`is_paused`]
    /// for how a caller confirms the pause actually landed rather than trusting this `Ok` alone.
    ///
    /// [`is_paused`]: AudioBackend::is_paused
    fn pause(&mut self) -> Result<()>;
    /// Whether the pipeline has actually reached `Paused` right now — `false` while a `pause()`
    /// is still asynchronously in flight and if it landed somewhere else entirely. A paused
    /// pipeline that is prerolling again after a seek counts as paused: nothing renders. Reads GStreamer's last-known state instantly and never blocks (unlike this
    /// crate's own tests' `wait_for_state_change`, which blocks on purpose — appropriate for a
    /// test, not for a call a caller might make from the GTK main thread).
    fn is_paused(&self) -> bool;
    /// Requires the pipeline to have already reached at least `PAUSED` (i.e. `play()` or
    /// `pause()` must have been called since the last `load()`) — GStreamer cannot seek a
    /// pipeline still in `NULL`/`READY`. Callers that want duration/position available before
    /// the user presses play should call `pause()` immediately after `load()`, which is also
    /// what unblocks seeking.
    fn seek(&mut self, position: Duration) -> Result<()>;
    /// Seeks to `position` (within the loaded file) at a new rate — GStreamer has no rate-only
    /// call, so a rate change is always a seek, and inherits `seek`'s same "at least PAUSED"
    /// requirement. The caller passes the position rather than this querying the pipeline's own:
    /// right after a flushing seek, or while a streamed seek is stalled, the pipeline reports the
    /// pre-seek position or none at all, and re-seeking to that would undo the seek or jump to the
    /// start of the file.
    fn set_speed(&mut self, speed: f64, position: Duration) -> Result<()>;
    fn position(&self) -> Option<Duration>;
    fn duration(&self) -> Option<Duration>;
    /// Non-blocking: returns the next pending bus event, if any, without waiting.
    fn poll_event(&self) -> Option<PlayerEvent>;
    /// Whether the **next** `load()` of a network-streamed URI should download ahead at full
    /// speed (bursting, then idling — better for battery) rather than trickling in at roughly the
    /// audio bitrate (the default; better for a slow or capped connection). Mirrors
    /// `apply_connection`'s "takes effect on the next load" contract — see `GstBackend`'s impl
    /// for why this can't be changed on an already-loaded pipeline. Local files are unaffected
    /// either way.
    fn set_burst_buffering(&mut self, enabled: bool);
    /// Whether a streamed file finished downloading ahead (in burst-buffering mode) since the
    /// last call — the moment the radio is still awake from a burst, so a small request now
    /// costs almost nothing. Fed by [`poll_event`](AudioBackend::poll_event)'s bus draining, so
    /// it only updates while events are polled. Backends that don't burst never report one.
    fn take_burst_finished(&mut self) -> bool {
        false
    }
    /// Releases whatever the pipeline currently holds — the audio device/stream and any open
    /// network connection — without expecting to resume from where it left off. A `set_state`
    /// call on a pipeline that has posted a bus `Error` never recovers it (GStreamer requires
    /// going through `Null` and reloading), so every caller that reaches an error, a failed
    /// `load()`, or a failed state change calls this rather than leaving a broken pipeline
    /// sitting on the audio server across every later Play/Retry — see this plan's Librem 5
    /// field report. The next real progress is always a fresh `load()`.
    fn reset(&mut self);
}

/// A `playbin`-based [`AudioBackend`]. Uses an explicit `fakesink` audio sink when constructed
/// via [`GstBackend::new_with_sink`] (used by this crate's own tests, where there is no real
/// audio device to render to); [`GstBackend::new`] uses the system default (`autoaudiosink`).
pub struct GstBackend {
    pipeline: gst::Element,
    current_speed: f64,
    /// Shared with the `source-setup` handler, which reads whatever was last applied here.
    /// GStreamer creates (and hands over) a fresh HTTP source per `load()`, so the properties
    /// can't be set on an element once — they have to be re-read at each source setup.
    connection_properties: std::sync::Arc<std::sync::Mutex<ConnectionProperties>>,
    /// Applied to `playbin`'s own `flags`/`ring-buffer-max-size` properties at the start of every
    /// `load()` (see `set_burst_buffering`'s doc comment for why it can't just be set once).
    /// Defaults to `true` — matches `PlaybackSettings::default()` — production always overrides
    /// this explicitly right after construction, same as `PlayerController`'s other
    /// settings-derived fields.
    burst_buffering: bool,
    /// Set by `poll_event` when `playbin`'s download buffer reports a finished download; read
    /// and cleared by `take_burst_finished`.
    burst_finished: std::cell::Cell<bool>,
}

impl GstBackend {
    pub fn new() -> Result<Self> {
        Self::build(None)
    }

    /// Build with an explicit audio sink element name (e.g. `"fakesink"`) instead of the system
    /// default — this is what makes this backend testable in a sandbox with no sound hardware.
    pub fn new_with_sink(sink_element_name: &str) -> Result<Self> {
        Self::build(Some(sink_element_name))
    }

    fn build(sink_element_name: Option<&str>) -> Result<Self> {
        // The player is built once the app has awaited `init_in_background`; building a pipeline
        // while the plugin registry is still loading would race it.
        debug_assert!(is_initialized(), "GStreamer isn't initialised yet");
        let pipeline = gst::ElementFactory::make("playbin").build()?;
        let connection_properties = std::sync::Arc::new(std::sync::Mutex::new(ConnectionProperties::default()));
        connect_source_setup(&pipeline, connection_properties.clone());
        match sink_element_name {
            Some(sink_name) => {
                let sink = gst::ElementFactory::make(sink_name).build()?;
                // Unlike most sinks, `fakesink` defaults `sync` to `false` — it would otherwise
                // render as fast as the CPU/network allow instead of at the pipeline clock's real
                // pace, which breaks any test that expects to observe mid-playback state (e.g.
                // pausing partway through a clip) rather than an already-finished one. Real sinks
                // (`autoaudiosink` et al.) already default `sync` to `true`, so this only changes
                // behavior for the test backend.
                sink.set_property("sync", true);
                pipeline.set_property("audio-sink", &sink);
            }
            // Real playback only (never the test `fakesink` path): a fallback for phone-call
            // interruption alongside ModemManager (see `docs/design/ui-spec.md`'s "Hardware
            // controls & interruptions") — tag this app's audio stream so PipeWire/WirePlumber's
            // own ducking policy can act on it even on a system with no ModemManager at all.
            // `autoaudiosink` resolves to a real sink lazily, once the pipeline actually starts,
            // so the property has to be set from `element-setup` (fired for every element it
            // creates internally), not up front.
            None => apply_stream_role_when_sink_is_ready(&pipeline),
        }
        Ok(Self { pipeline, current_speed: 1.0, connection_properties, burst_buffering: true, burst_finished: std::cell::Cell::new(false) })
    }

    fn bus(&self) -> gst::Bus {
        self.pipeline.bus().expect("a playbin pipeline always has a bus")
    }
}

/// Wires `playbin`'s `source-setup` signal: whatever HTTP source GStreamer creates for the next
/// `load()` gets the connection's transport properties applied — but only the properties that
/// source actually has, so a local `file://` source (no `user-agent`, no `ssl-strict`) is never
/// touched.
///
/// Streaming can't present a client certificate: `souphttpsrc` takes one only through its
/// `tls-interaction` property, a `GTlsInteraction` whose `request_certificate` supplies it, and
/// gio-rs can't subclass `GTlsInteraction` yet (not in 0.22.10, the latest at the time of
/// writing). Once it can, build the interaction from the server's PKCS#12 bundle
/// (`gio::TlsCertificate::from_pkcs12`), carry the bundle in [`ConnectionProperties`] and set
/// `tls-interaction` here; the mutual-TLS case in `tests/streaming_options.rs` then becomes a
/// passing one. Until then a server that requires a certificate fails the request visibly.
fn connect_source_setup(pipeline: &gst::Element, properties: std::sync::Arc<std::sync::Mutex<ConnectionProperties>>) {
    pipeline.connect("source-setup", false, move |values| {
        let source = values[1].get::<gst::Element>().expect("source-setup signal carries the source element");
        let properties = properties.lock().expect("connection properties mutex");

        if source.has_property("ssl-strict", Some(glib::Type::BOOL)) {
            source.set_property("ssl-strict", properties.ssl_strict);
        }
        if let Some(user_agent) = &properties.user_agent {
            if source.has_property("user-agent", Some(glib::Type::STRING)) {
                source.set_property("user-agent", user_agent.as_str());
            }
        }
        // `extra-headers` is souphttpsrc's property for this; an earlier version asked for a
        // non-existent `http-headers`, which the `has_property` guard turned into a silent no-op,
        // so a header-protected server never saw its header on a stream (found by
        // `abs-player/tests/streaming_options.rs`).
        if !properties.extra_headers.is_empty() && source.has_property("extra-headers", Some(gst::Structure::static_type())) {
            let mut headers = gst::Structure::new_empty("extra-headers");
            for (name, value) in &properties.extra_headers {
                headers.set(name.as_str(), value.as_str());
            }
            source.set_property("extra-headers", headers);
        }
        None
    });
}

impl AudioBackend for GstBackend {
    fn load(&mut self, uri: &str) -> Result<()> {
        self.pipeline.set_state(gst::State::Null)?;
        apply_burst_buffering(&self.pipeline, self.burst_buffering);
        self.pipeline.set_property("uri", uri);
        self.current_speed = 1.0;
        Ok(())
    }

    fn apply_connection(&mut self, properties: &ConnectionProperties) {
        *self.connection_properties.lock().expect("connection properties mutex") = properties.clone();
    }

    fn play(&mut self) -> Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        self.pipeline.set_state(gst::State::Paused)?;
        Ok(())
    }

    fn is_paused(&self) -> bool {
        // `current_state() == Paused` means the pipeline committed Paused: the clock is stopped
        // and nothing renders until it commits Playing. A flushing seek (or a rate change) on a
        // paused pipeline leaves it there with `pending == Paused` while it prerolls again — on
        // a stream, for as long as the new HTTP range request takes — and that is still paused.
        // What isn't: a pipeline still at `Playing` (a pause in flight or stuck in `Async`), or
        // one already on its way back to `Playing`. Both reads are non-blocking.
        self.pipeline.current_state() == gst::State::Paused && self.pipeline.pending_state() != gst::State::Playing
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        let clock_time = gst::ClockTime::from_nseconds(position.as_nanos() as u64);
        let ok = self.pipeline.seek(
            self.current_speed,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            clock_time,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
        if ok.is_err() {
            return Err(PlayerError::SeekFailed);
        }
        Ok(())
    }

    fn set_speed(&mut self, speed: f64, position: Duration) -> Result<()> {
        // GStreamer has no standalone "set rate" call — a rate change is expressed as a seek with
        // a new rate, to the position the caller knows is right (see the trait's doc comment).
        // The rate is only kept if that seek was accepted: every later seek reuses
        // `current_speed`, which would otherwise quietly apply a rate the caller was told failed.
        // GStreamer rejects a rate of zero (and has no use for NaN or infinity); say so here
        // rather than record a rate every later seek would then fail with.
        if !speed.is_finite() || speed <= 0.0 {
            return Err(PlayerError::SeekFailed);
        }
        let previous = self.current_speed;
        self.current_speed = speed;
        let result = self.seek(position);
        if result.is_err() {
            self.current_speed = previous;
        }
        result
    }

    fn position(&self) -> Option<Duration> {
        self.pipeline
            .query_position::<gst::ClockTime>()
            .map(|t| Duration::from_nanos(t.nseconds()))
    }

    fn duration(&self) -> Option<Duration> {
        self.pipeline
            .query_duration::<gst::ClockTime>()
            .map(|t| Duration::from_nanos(t.nseconds()))
    }

    fn poll_event(&self) -> Option<PlayerEvent> {
        // `playbin` posts plenty of bus messages besides `Eos`/`Error` (state changes,
        // buffering, tags, ...). A caller polling once per tick must not have a real `Eos`
        // stuck behind an uninteresting message for another whole tick interval, so drain the
        // bus here until an event worth reporting turns up or the bus is actually empty.
        let bus = self.bus();
        loop {
            let msg = bus.pop()?;
            match msg.view() {
                gst::MessageView::Eos(_) => return Some(PlayerEvent::EndOfStream),
                gst::MessageView::Error(e) => {
                    let error = e.error();
                    let kind = classify_bus_error(&error, e.debug().as_deref());
                    // An HTTP source's messages quote the stream URL, which carries the access
                    // token as a query parameter — and these strings reach the log and the error
                    // banner's details.
                    return Some(PlayerEvent::Error(PlaybackError {
                        kind,
                        message: redact_tokens(&error.to_string()),
                        debug: e.debug().map(|d| redact_tokens(&d)),
                    }));
                }
                // In download (burst) mode `playbin` fetches the file through a `downloadbuffer`,
                // which posts this once the whole file is in. Its buffering messages are no use
                // here: they reach 100% as soon as there's enough to play through, well before
                // the download ends (checked against a throttled HTTP source).
                gst::MessageView::Element(element) if element.structure().is_some_and(|s| s.name() == DOWNLOAD_COMPLETE_MESSAGE) => {
                    self.burst_finished.set(true);
                    continue;
                }
                _ => continue,
            }
        }
    }

    fn take_burst_finished(&mut self) -> bool {
        self.burst_finished.replace(false)
    }

    fn set_burst_buffering(&mut self, enabled: bool) {
        self.burst_buffering = enabled;
        // Deliberately not applied to `self.pipeline` here: `playbin`'s buffering strategy is
        // read once as the pipeline transitions out of `NULL`, so changing it while a stream is
        // already loaded/playing wouldn't retroactively change how it's fetching — `load()` is
        // the only point this can take effect, same contract as `apply_connection`.
    }

    fn reset(&mut self) {
        // Same effect as `Drop`'s own teardown, just without dropping the pipeline itself — this
        // is what releases the `pulsesink` stream and any open HTTP connection after an error,
        // so the device is never left held by a pipeline nothing will ever resume without a
        // fresh `load()`. `Null` is always a synchronous, unconditionally successful transition
        // for `playbin` (unlike `Playing`/`Paused`, which can go `Async`), so this needs no
        // result check.
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for GstBackend {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// The `GST_PLAY_FLAG_DOWNLOAD` bit of `playbin`'s `flags` property (a plugin-defined
/// `GstPlayFlags`, not part of core GStreamer, so gstreamer-rs has no static binding for it —
/// see `apply_burst_buffering`). Value from `playbin`'s own flag ordering: VIDEO, AUDIO, TEXT,
/// VIS, SOFT_VOLUME, NATIVE_AUDIO, NATIVE_VIDEO, DOWNLOAD is the 8th (bit 7).
const GST_PLAY_FLAG_DOWNLOAD: u32 = 1 << 7;

/// The element message `downloadbuffer` (what `playbin` fetches through in download mode) posts
/// when the whole file has been downloaded.
const DOWNLOAD_COMPLETE_MESSAGE: &str = "GstCacheDownloadComplete";

/// 64 MiB — generous for an audiobook chapter (typically a few MB to a few tens of MB at
/// spoken-word bitrates), so download mode can fetch a whole file ahead and let the radio idle,
/// without holding an unbounded amount of a very long chapter in memory/temp storage at once.
const BURST_RING_BUFFER_BYTES: u64 = 64 * 1024 * 1024;

/// Applies (or clears) `playbin`'s download-buffering mode: with it on, the HTTP source fetches
/// ahead at full speed into a ring buffer instead of trickling in at roughly the audio bitrate —
/// better for battery (the radio gets to idle between bursts) at the cost of downloading data
/// that might go unlistened if playback stops early. `flags` is a `GstPlayFlags`, a type the
/// `playbin` element itself registers at runtime — gstreamer-rs has no static Rust binding for it
/// (unlike `gst::State` et al.), so this goes through `glib::FlagsClass` generically, the same way
/// the upstream gstreamer-rs `playbin` examples do. Silently leaves the setting unapplied (logged)
/// if `flags` isn't actually a flags-typed property — would mean a `playbin` factory that changed
/// shape in a way this code doesn't understand, not something a user action can trigger.
fn apply_burst_buffering(pipeline: &gst::Element, enabled: bool) {
    let flags_value = pipeline.property_value("flags");
    let Some(flags_class) = glib::FlagsClass::with_type(flags_value.type_()) else {
        tracing::warn!("playbin's \"flags\" property isn't a flags type; couldn't apply the burst-buffering setting");
        return;
    };
    let updated = if enabled { flags_class.set(flags_value, GST_PLAY_FLAG_DOWNLOAD) } else { flags_class.unset(flags_value, GST_PLAY_FLAG_DOWNLOAD) };
    match updated {
        Ok(value) => {
            pipeline.set_property_from_value("flags", &value);
            pipeline.set_property("ring-buffer-max-size", if enabled { BURST_RING_BUFFER_BYTES } else { 0 });
        }
        Err(_) => tracing::warn!("couldn't set playbin's download-buffering flag"),
    }
}

/// Tags this app's audio stream with PulseAudio's `media.role=music` once `playbin`'s internal
/// `autoaudiosink` actually resolves to a real sink — confirmed via `gst-inspect-1.0 pulsesink`
/// that `stream-properties` is the right property name (a `GstStructure`, not a plain string map).
/// Also sets `client-name`, a separate `pulsesink` property (the *client's* PulseAudio identity,
/// distinct from the per-stream `stream-properties`) — this is what makes this app's audio
/// stream identifiable in `pactl list clients`/`pw-top` rather than showing up as an anonymous
/// GStreamer client, which is what actually makes a future "why is the audio stack stuck" report
/// diagnosable (see this plan's Librem 5 field report). `pipewiresink` wasn't installed in the
/// environment this was implemented in to cross-check against, so this only reaches PipeWire
/// installs that route through `pulsesink`'s PulseAudio compatibility layer (the common case); a
/// native `pipewiresink` deployment is unverified and should be checked with
/// `gst-inspect-1.0 pipewiresink` on real target hardware.
fn apply_stream_role_when_sink_is_ready(pipeline: &gst::Element) {
    use glib::prelude::ObjectExt;
    // `element-setup` fires from whichever internal GStreamer thread creates the element (caught
    // live: a real run aborted with "thread caused non-unwinding panic" under `connect_local`,
    // whose thread-guard requires same-thread emission) — this closure captures nothing, so the
    // thread-safe `connect` costs nothing and is the only correct choice here.
    pipeline.connect("element-setup", false, |values| {
        let element = values.get(1)?.get::<gst::Element>().ok()?;
        if element.factory().map(|f| f.name() == "pulsesink").unwrap_or(false) {
            let props = gst::Structure::builder("props").field("media.role", "music").build();
            element.set_property("stream-properties", &props);
            if element.has_property("client-name", Some(glib::Type::STRING)) {
                element.set_property("client-name", "Audiobooklet");
            }
        }
        None
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn background_init_reports_ready_to_every_waiter_and_its_timing() {
        let (took_tx, took_rx) = std::sync::mpsc::channel();
        let init = init_in_background(move |took, outcome| {
            let _ = took_tx.send((took, outcome.clone()));
        });
        futures::executor::block_on(async {
            assert_eq!(init.wait().await, Ok(()));
            // A second wait, and a clone's, answer at once with the same outcome.
            assert_eq!(init.wait().await, Ok(()));
            assert_eq!(init.clone().wait().await, Ok(()));
        });
        assert!(init.is_finished());
        let (_, outcome) = took_rx.recv_timeout(Duration::from_secs(5)).expect("the timing callback ran");
        assert_eq!(outcome, Ok(()));
        // And GStreamer is really usable afterwards.
        assert!(gst::ElementFactory::make("fakesink").build().is_ok());
    }

    #[test]
    fn a_panicking_initialisation_reaches_the_waiter_as_an_error_and_never_counts_as_finished() {
        let init = init_in_background_with(|| panic!("plugin blew up"), |_, _| {});
        let outcome = futures::executor::block_on(init.wait());
        assert!(outcome.unwrap_err().contains("ended without a result"));
        // Not "finished": the thread died before it could say so. Exit handling bounds its wait.
        assert!(!init.is_finished());
    }

    #[test]
    fn background_init_failure_reaches_the_waiter_and_a_wait_before_the_end_does_not_block() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let init = init_in_background_with(
            move || {
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                Err("no plugins".to_string())
            },
            |_, _| {},
        );
        assert!(!init.is_finished(), "the work is gated, so it can't have finished");
        // Polling once while it is pending returns Pending instead of blocking the thread.
        let waiting = init.clone();
        let mut future = Box::pin(async move { waiting.wait().await });
        let pending = futures::executor::block_on(async { futures::poll!(&mut future).is_pending() });
        assert!(pending);
        release_tx.send(()).unwrap();
        assert_eq!(futures::executor::block_on(future), Err("no plugins".to_string()));
    }

    /// Write a short, silent WAV file and return its `file://` URI, so tests exercise a real
    /// GStreamer decode/demux pipeline without needing a network fetch or a bundled fixture.
    fn silent_wav_uri(tmp: &tempfile::TempDir, seconds: u32) -> String {
        let path: PathBuf = tmp.path().join("silence.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..(spec.sample_rate * seconds) {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
        format!("file://{}", path.display())
    }

    /// `connect_source_setup` guards each property with `has_property`, so a misspelt name is a
    /// silent no-op (custom headers never reached a stream that way once). Pins the names against
    /// the real element. Skipped where the HTTP source plugin isn't installed.
    #[test]
    fn souphttpsrc_has_every_property_the_source_setup_sets() {
        init().expect("gstreamer should initialize in this environment");
        let Ok(source) = gst::ElementFactory::make("souphttpsrc").build() else { return };
        for (name, ty) in [
            ("ssl-strict", glib::Type::BOOL),
            ("user-agent", glib::Type::STRING),
            ("extra-headers", gst::Structure::static_type()),
        ] {
            assert!(source.has_property(name, Some(ty)), "souphttpsrc has no `{name}` property of the expected type");
        }
    }

    /// Serves `bytes` over plain HTTP on a local port, slowly (about 1 MB/s), so a download
    /// takes long enough to tell "enough buffered" from "fully downloaded". Returns the URL.
    fn serve_slowly(bytes: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let head = format!("HTTP/1.1 200 OK\r\nContent-Type: audio/x-wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
                if stream.write_all(head.as_bytes()).is_err() {
                    continue;
                }
                for chunk in bytes.chunks(64 * 1024) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
            }
        });
        format!("http://127.0.0.1:{port}/a.wav")
    }

    /// Polls events for up to `within`, returning whether a finished burst was reported.
    fn burst_reported(player: &mut GstBackend, within: std::time::Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < within {
            while player.poll_event().is_some() {}
            if player.take_burst_finished() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    /// A streamed file reports a finished burst once it has fully downloaded — only in burst
    /// mode, and only once.
    #[test]
    fn a_finished_download_is_reported_once_in_burst_mode_only() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = std::fs::read(silent_wav_uri(&tmp, 60).trim_start_matches("file://")).unwrap();
        let url = serve_slowly(wav);
        init().expect("gstreamer should initialize in this environment");
        if gst::ElementFactory::find("souphttpsrc").is_none() {
            return;
        }

        let mut player = backend();
        player.set_burst_buffering(true);
        player.load(&url).unwrap();
        player.play().unwrap();
        assert!(burst_reported(&mut player, std::time::Duration::from_secs(15)), "the download should finish and be reported");
        assert!(!player.take_burst_finished(), "reported once");
        player.reset();

        let mut player = backend();
        player.set_burst_buffering(false);
        player.load(&url).unwrap();
        player.play().unwrap();
        assert!(!burst_reported(&mut player, std::time::Duration::from_secs(3)), "streaming without bursts never reports one");
        player.reset();
    }

    fn backend() -> GstBackend {
        init().expect("gstreamer should initialize in this environment");
        GstBackend::new_with_sink("fakesink").expect("build a playbin with a fake sink")
    }

    /// Block until the pipeline leaves the ASYNC state (i.e. `PAUSED`/`PLAYING` actually took
    /// effect), so position/duration queries below aren't racing pipeline startup.
    fn wait_for_state_change(backend: &GstBackend) {
        let (_result, _current, _pending) = backend
            .pipeline
            .state(gst::ClockTime::from_seconds(5));
    }

    #[test]
    fn load_then_play_reaches_the_playing_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 2)).unwrap();
        player.play().unwrap();
        wait_for_state_change(&player);

        assert_eq!(player.pipeline.current_state(), gst::State::Playing);
    }

    #[test]
    fn pause_reaches_the_paused_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 2)).unwrap();
        player.play().unwrap();
        wait_for_state_change(&player);

        player.pause().unwrap();
        wait_for_state_change(&player);

        assert_eq!(player.pipeline.current_state(), gst::State::Paused);
    }

    /// Pins `is_paused()`'s whole contract — the gap `pause()`'s own doc comment describes: `Ok`
    /// from `pause()` means only that GStreamer *accepted* the request, not that it landed.
    /// `is_paused()` is what tells those apart, and it must never block (checked here by never
    /// calling `wait_for_state_change` before the first assertion — if `is_paused()` secretly
    /// blocked until the pipeline settled, this test would still pass, but a real caller on the
    /// GTK main thread would freeze on every pause).
    #[test]
    fn is_paused_distinguishes_an_in_flight_pause_from_a_landed_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 2)).unwrap();
        player.play().unwrap();
        wait_for_state_change(&player);
        assert!(!player.is_paused(), "still playing — must not report paused");

        player.pause().unwrap();
        // Deliberately no `wait_for_state_change` here: this is the exact moment production
        // code's own `pause()` call returns, before anything has had a chance to settle.
        // Whether this pipeline happens to reach `Paused` synchronously or is still `Async` is
        // itself timing-dependent (and not the point) — either way `is_paused()` must return an
        // instant, non-blocking answer.
        let _ = player.is_paused();

        wait_for_state_change(&player);
        assert!(player.is_paused(), "settled in Paused — must report paused");
    }

    /// A seek while paused makes the pipeline preroll again (GStreamer's "lost state": current
    /// and pending both `Paused`). Nothing renders meanwhile, so it is still paused — reporting
    /// otherwise made the app's pause confirmation tear down a perfectly paused pipeline after
    /// pause → rewind on a slow stream.
    #[test]
    fn a_seek_while_paused_is_still_paused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 5)).unwrap();
        player.play().unwrap();
        wait_for_state_change(&player);
        player.pause().unwrap();
        wait_for_state_change(&player);
        assert!(player.is_paused());

        player.seek(Duration::from_secs(2)).unwrap();
        assert!(player.is_paused(), "re-prerolling after a seek is still paused");
        player.set_speed(1.5, Duration::from_secs(1)).unwrap();
        assert!(player.is_paused(), "re-prerolling after a rate change is still paused");
        wait_for_state_change(&player);
        assert!(player.is_paused());
    }

    #[test]
    fn duration_matches_the_generated_files_length() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 3)).unwrap();
        player.pause().unwrap(); // PAUSED is enough for GStreamer to know the duration
        wait_for_state_change(&player);

        let duration = player.duration().expect("duration should be known once PAUSED");
        // Allow slack: WAV header/container overhead can shift this by a few ms.
        assert!(
            (duration.as_secs_f64() - 3.0).abs() < 0.2,
            "expected ~3s, got {duration:?}"
        );
    }

    #[test]
    fn seek_moves_the_reported_position() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 5)).unwrap();
        player.pause().unwrap();
        wait_for_state_change(&player);

        player.seek(Duration::from_secs(2)).unwrap();
        wait_for_state_change(&player);

        let position = player.position().expect("position should be known after seeking");
        assert!(
            (position.as_secs_f64() - 2.0).abs() < 0.2,
            "expected ~2s, got {position:?}"
        );
    }

    #[test]
    fn seek_before_loading_anything_fails_rather_than_panicking() {
        let mut player = backend();
        // No `load()` call: playbin has no URI set, so a seek must error, not panic.
        let result = player.seek(Duration::from_secs(1));
        assert!(result.is_err());
    }

    #[test]
    fn an_unusable_speed_is_refused_and_not_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 5)).unwrap();
        player.pause().unwrap();
        wait_for_state_change(&player);
        for speed in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(player.set_speed(speed, Duration::from_secs(1)).is_err(), "{speed} should be refused");
            assert_eq!(player.current_speed, 1.0);
        }
        assert!(player.seek(Duration::from_secs(1)).is_ok(), "later seeks must be unaffected");
    }

    #[test]
    fn set_speed_preserves_position() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 5)).unwrap();
        player.pause().unwrap();
        wait_for_state_change(&player);
        player.seek(Duration::from_secs(2)).unwrap();
        wait_for_state_change(&player);

        player.set_speed(1.5, Duration::from_secs(2)).unwrap();
        wait_for_state_change(&player);

        let position = player.position().unwrap();
        assert!(
            (position.as_secs_f64() - 2.0).abs() < 0.3,
            "changing speed should not itself jump position, got {position:?}"
        );
    }

    #[test]
    fn end_of_stream_is_reported_via_poll_event() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        // A very short clip so EOS arrives quickly.
        player.load(&silent_wav_uri(&tmp, 1)).unwrap();
        player.play().unwrap();

        let mut saw_eos = false;
        for _ in 0..100 {
            if let Some(PlayerEvent::EndOfStream) = player.poll_event() {
                saw_eos = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(saw_eos, "expected an EndOfStream event within 5s");
    }

    /// A real pipeline error, not a synthetic `glib::Error` — loading a file that doesn't exist
    /// makes a real `GstBackend` post a genuine bus error, proving `poll_event` actually classifies
    /// what GStreamer sends, not just what `classify_gst_error`'s unit tests construct by hand.
    #[test]
    fn a_missing_file_is_reported_as_a_classified_error() {
        let mut player = backend();
        player.load("file:///nonexistent/does-not-exist.wav").unwrap();
        // A missing local file can fail synchronously right out of `set_state` (unlike a network
        // 404, which only fails once the async pipeline actually tries to read) — either way, a
        // bus `Error` message follows, which is what this test is actually about.
        let _ = player.play();

        let mut error = None;
        for _ in 0..100 {
            if let Some(PlayerEvent::Error(err)) = player.poll_event() {
                error = Some(err);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let error = error.expect("expected an Error event within 5s");
        assert_eq!(error.kind, PlaybackErrorKind::ResourceNotFound, "got: {error:?}");
        assert!(!error.message.is_empty(), "the raw message must never be empty — this is the 'don't hide what happened' text");
    }

    /// `classify_gst_error` is a pure function of a `glib::Error`'s domain/code — these construct
    /// one directly per GStreamer error kind rather than needing a real pipeline to fail in every
    /// possible way, so every branch is covered deterministically.
    #[test]
    fn classifies_missing_plugin_as_missing_codec() {
        let err = glib::Error::new(gst::CoreError::MissingPlugin, "no h264 decoder");
        assert_eq!(classify_gst_error(&err), PlaybackErrorKind::MissingCodec);
    }

    #[test]
    fn classifies_codec_not_found_as_missing_codec() {
        let err = glib::Error::new(gst::StreamError::CodecNotFound, "no aac decoder");
        assert_eq!(classify_gst_error(&err), PlaybackErrorKind::MissingCodec);
    }

    #[test]
    fn classifies_decode_failure_as_unsupported_or_corrupt() {
        let err = glib::Error::new(gst::StreamError::Decode, "malformed stream");
        assert_eq!(classify_gst_error(&err), PlaybackErrorKind::UnsupportedOrCorrupt);
    }

    #[test]
    fn classifies_resource_not_found_as_resource_not_found() {
        let err = glib::Error::new(gst::ResourceError::NotFound, "404");
        assert_eq!(classify_gst_error(&err), PlaybackErrorKind::ResourceNotFound);
    }

    #[test]
    fn classifies_not_authorized_as_not_authorized() {
        let err = glib::Error::new(gst::ResourceError::NotAuthorized, "401");
        assert_eq!(classify_gst_error(&err), PlaybackErrorKind::NotAuthorized);
    }

    #[test]
    fn a_generic_failure_from_the_http_source_is_a_network_error() {
        let err = glib::Error::new(gst::StreamError::Failed, "Internal data stream error.");
        let debug = "../libs/gst/base/gstbasesrc.c(3132): gst_base_src_loop (): /GstPlayBin:playbin0/GstURIDecodeBin:uridecodebin0/GstSoupHTTPSrc:source:\nstreaming stopped, reason error (-5)";
        assert_eq!(classify_bus_error(&err, Some(debug)), PlaybackErrorKind::Network);
        assert_eq!(classify_bus_error(&err, Some("/GstPlayBin:playbin0/GstFileSrc:source: streaming stopped")), PlaybackErrorKind::Other, "a local file's failure is not a network one");
        assert_eq!(classify_bus_error(&err, None), PlaybackErrorKind::Other);
    }

    #[test]
    fn classifies_open_write_as_audio_output() {
        let err = glib::Error::new(gst::ResourceError::OpenWrite, "could not open audio device");
        assert_eq!(classify_gst_error(&err), PlaybackErrorKind::AudioOutput);
    }

    #[test]
    fn classifies_read_failure_as_network() {
        let err = glib::Error::new(gst::ResourceError::Read, "connection reset");
        assert_eq!(classify_gst_error(&err), PlaybackErrorKind::Network);
    }

    #[test]
    fn classifies_unmatched_core_error_as_other() {
        let err = glib::Error::new(gst::CoreError::Negotiation, "caps negotiation failed");
        assert_eq!(classify_gst_error(&err), PlaybackErrorKind::Other);
    }

    #[test]
    fn player_error_kinds_map_to_the_right_buckets() {
        assert_eq!(PlayerError::NoSourceLoaded.kind(), PlaybackErrorKind::Unavailable);
        assert_eq!(PlayerError::SeekFailed.kind(), PlaybackErrorKind::Other);
    }

    #[test]
    fn loading_a_new_source_resets_speed_to_normal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut player = backend();
        player.load(&silent_wav_uri(&tmp, 2)).unwrap();
        player.pause().unwrap(); // a seek (which set_speed performs) needs at least PAUSED
        wait_for_state_change(&player);
        player.set_speed(2.0, Duration::ZERO).unwrap();
        assert_eq!(player.current_speed, 2.0);

        player.load(&silent_wav_uri(&tmp, 2)).unwrap();
        assert_eq!(player.current_speed, 1.0, "a fresh load should not inherit the old speed");
    }

    #[test]
    fn redact_tokens_hides_every_token_value_and_keeps_the_rest() {
        assert_eq!(
            redact_tokens("Could not read from resource. URL: https://abs.example/api/items/i/file/1?token=eyJ.abc-123&x=1 (x)"),
            "Could not read from resource. URL: https://abs.example/api/items/i/file/1?token=REDACTED&x=1 (x)"
        );
        assert_eq!(redact_tokens("a?token=one b?token=two"), "a?token=REDACTED b?token=REDACTED");
        assert_eq!(redact_tokens("ends with token="), "ends with token=");
        assert_eq!(redact_tokens("no secrets here"), "no secrets here");
    }

    #[test]
    fn init_can_be_called_more_than_once() {
        init().unwrap();
        init().unwrap();
    }

    /// Confirms the `media.role=music` stream-role fallback actually reaches a real `pulsesink`
    /// once `autoaudiosink` resolves to one — needs a real, running PulseAudio (or a PipeWire
    /// install routing through its PulseAudio compatibility layer), which this sandbox doesn't
    /// have (`pactl` isn't even installed here). Run explicitly once such an environment is
    /// available: `cargo test -p abs-player -- --ignored stream_role`.
    #[test]
    #[ignore]
    fn real_backend_tags_the_stream_with_a_music_role() {
        let tmp = tempfile::tempdir().unwrap();
        init().unwrap();
        let mut player = GstBackend::new().expect("build a real playbin");
        player.load(&silent_wav_uri(&tmp, 2)).unwrap();
        player.play().unwrap();
        wait_for_state_change(&player);

        // `element-setup` fires as soon as `autoaudiosink` picks its real child, which happens
        // during the state change above — by now the sink (if it's `pulsesink`) should already
        // carry `stream-properties`.
        let bin = player.pipeline.clone().downcast::<gst::Bin>().unwrap();
        let sink = bin
            .iterate_recurse()
            .into_iter()
            .flatten()
            .find(|e| e.factory().map(|f| f.name() == "pulsesink").unwrap_or(false))
            .expect("expected a pulsesink somewhere in the pipeline");
        let props = sink.property::<gst::Structure>("stream-properties");
        assert_eq!(props.get::<String>("media.role").unwrap(), "music");
    }
}
