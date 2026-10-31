//! Headphone plug/unplug watching — `docs/design/ui-spec.md`'s "Hardware controls & interruptions":
//! pause playback when the audio output the listener is wearing goes away (wired jack unplugged,
//! Bluetooth headphones disconnected), and optionally resume when it comes back. Lives in
//! `abs-player` for the same reason `mpris` and `call_watch` do — this is real OS audio-server
//! integration, not a GTK widget-toolkit concern.
//!
//! MPRIS can't do any of this: it is an outbound-only control interface (it exposes this app's
//! player state and accepts transport commands from the shell), with no concept of inbound
//! audio-routing events. The routing authority is the audio server — PulseAudio, or PipeWire
//! via its `pipewire-pulse` compatibility socket (the standard mobile-Linux setup) — which
//! reports kernel jack detection as a *port availability* flag on the sink's ports. This module
//! watches that over the PulseAudio client protocol (the same libpulse GStreamer's `pulsesink`
//! — the audio path this app actually plays through — already uses), so it sees both wired jack
//! events and Bluetooth sink appearances/disappearances with one integration.
//!
//! Port availability is tracked **per sink** and the events describe the aggregate — "is any
//! headphone output still available?" — because more than one sink can carry a headphone-ish
//! port at once: PulseAudio's Bluetooth sinks have `headphone-output` / `headset-output` ports
//! whose availability follows the A2DP/HFP transport state (yes while streaming, unknown while
//! idle, no while disconnected) alongside the wired card's `[Out] Headphones` / `analog-output-
//! headphones` port. Feeding those readings into one shared state would flip-flop on every
//! rescan (a volume change is enough to trigger one) and pause playback for no reason; and a
//! wired unplug while Bluetooth headphones are still streaming is not an "output went away".
//!
//! Two known limits, both documented in the spec: a Bluetooth disconnect shows up as the whole
//! sink being removed (always detectable), while a wired unplug needs the hardware to report jack
//! detection — devices whose port reports "unknown" availability (some USB DACs) can't be
//! detected and are ignored. And like `call_watch`, there is deliberately no unconditional
//! auto-resume: resuming after a replug only ever happens when the preceding pause was itself
//! caused by an unplug (see `PlayerController::handle_route_event`), so a manual pause or a
//! phone call is never overridden by a reconnection.
//!
//! The connection to the audio server is kept for the app's lifetime: if it drops (PulseAudio
//! restarted or crashed, which on a phone that suspends/resumes for days is a real possibility),
//! the watcher reconnects with backoff and starts observing from scratch — only the *first*
//! connection attempt is allowed to fail `new()`, so an environment with no audio server at all
//! still degrades gracefully.
//!
//! Delivered events:
//! - [`RouteEvent::Unplugged`] — the headphone port flipped from available to unavailable, or
//!   the sink we were listening on disappeared (Bluetooth).
//! - [`RouteEvent::Replugged`] — a headphone port became available again. Harmless if nothing
//!   had been unplugged: consumers gate on "did we auto-pause".

/// A change in the availability of the headphone output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteEvent {
    /// The headphone output went away (jack unplugged, Bluetooth device disconnected).
    Unplugged,
    /// A headphone output (re)appeared.
    Replugged,
}

/// Watches for headphone plug/unplug. Implemented by [`PulseRouteWatcher`] for real systems
/// and a `FakeRouteWatcher` (test-only, in this module's tests) for the dependency-injection
/// pattern this crate already uses for [`crate::AudioBackend`] and [`crate::call_watch::CallWatcher`].
pub trait RouteWatcher {
    /// Registers the callback and starts delivering events. Calling this more than once replaces
    /// any previously registered callback (mirrors `CallWatcher`'s single-owner shape — there is
    /// only ever one thing that should react to a route change).
    fn start(&mut self, on_event: Box<dyn Fn(RouteEvent)>);
}

#[derive(Debug, thiserror::Error)]
pub enum RouteWatchError {
    #[error("couldn't connect to the audio server (PulseAudio/PipeWire): {0}")]
    NoAudioServer(String),
}

/// Turns raw audio-server observations into [`RouteEvent`]s — kept as a pure, synchronous state
/// machine so the classification rules are unit-testable without any audio server at all.
///
/// Each sink that has a headphone-ish port with a *known* availability is tracked separately (see
/// the module docs for why: Bluetooth sinks carry such ports too), and the events describe the
/// aggregate `headphones_present: Option<bool>` — "is any headphone output available?", where
/// `None` means "nothing observed yet / no sink with a known headphone port". The asymmetry in
/// the transitions is deliberate and load-bearing:
/// - `None -> true` **does** emit `Replugged`: it is what a Bluetooth reconnection looks like
///   (the sink vanished, wiping our knowledge; its return re-reports port availability). It is
///   safe even at startup because consumers only act on `Replugged` when a previous unplug was
///   the reason for the current pause.
/// - `None -> false` emits **nothing**: at startup that is just "headphones are (still)
///   unplugged", and pausing over the state of the world at launch would be absurd.
/// - `true -> None` (the last sink with headphones went away) **does** emit `Unplugged`.
/// - `Unknown` port availability (devices without jack detection, an idle Bluetooth transport)
///   leaves that sink's last known state untouched.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RouteClassifier {
    /// Per sink index: whether that sink's headphone port is currently available. Only sinks with
    /// a headphone-ish port of known availability are in here.
    present_by_sink: std::collections::BTreeMap<u32, bool>,
    /// The aggregate last reported, against which the next transition is measured.
    headphones_present: Option<bool>,
}

/// What a sink's headphone port reports — PulseAudio's `pa_available_t`, minus its numeric
/// identity. `Unknown` means the hardware doesn't do jack detection at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortAvailability {
    Unknown,
    Yes,
    No,
}

/// One sink as seen by a scan of the audio server's sink list: its index (PulseAudio's stable
/// per-sink identity, also what sink-removed events carry) and what its headphone port reports —
/// `None` when the sink has no headphone-ish port at all (speakers, HDMI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SinkReading {
    pub(crate) index: u32,
    pub(crate) headphones: Option<PortAvailability>,
}

impl RouteClassifier {
    fn aggregate(&self) -> Option<bool> {
        if self.present_by_sink.is_empty() {
            None
        } else {
            Some(self.present_by_sink.values().any(|&present| present))
        }
    }

    /// Re-derives the aggregate and reports the transition from the last reported one.
    fn transition(&mut self) -> Option<RouteEvent> {
        let next = self.aggregate();
        let event = match (self.headphones_present, next) {
            (Some(true), Some(false)) | (Some(true), None) => Some(RouteEvent::Unplugged),
            (Some(false), Some(true)) | (None, Some(true)) => Some(RouteEvent::Replugged),
            _ => None,
        };
        self.headphones_present = next;
        event
    }

    /// Feeds one complete scan of the sink list. Sinks absent from the scan are forgotten (they
    /// are gone); a sink whose headphone port reads `Unknown` keeps whatever was last known
    /// about it; a sink without a headphone port is ignored.
    pub(crate) fn on_scan(&mut self, readings: &[SinkReading]) -> Option<RouteEvent> {
        let mut next = std::collections::BTreeMap::new();
        for reading in readings {
            match reading.headphones {
                Some(PortAvailability::Yes) => {
                    next.insert(reading.index, true);
                }
                Some(PortAvailability::No) => {
                    next.insert(reading.index, false);
                }
                Some(PortAvailability::Unknown) => {
                    if let Some(&present) = self.present_by_sink.get(&reading.index) {
                        next.insert(reading.index, present);
                    }
                }
                None => {}
            }
        }
        self.present_by_sink = next;
        self.transition()
    }

    /// Feeds "sink `index` disappeared" — how a Bluetooth disconnect manifests. An unplug is
    /// reported only if that sink was the last one with headphones present; the sink's return
    /// re-reports availability starting from scratch.
    pub(crate) fn on_sink_removed(&mut self, index: u32) -> Option<RouteEvent> {
        self.present_by_sink.remove(&index);
        self.transition()
    }
}

/// The real watcher: a background thread owning a libpulse context, subscribed to sink, card and
/// server events. Introspection results and subscribe events are funneled through a channel and
/// classified between `iterate()` calls (a libpulse callback can't re-enter the context that owns
/// it, so nothing inside the callback ever touches `Context` — it only sends). Classified events
/// cross into the GLib main loop over a `futures` channel, whose receiving future runs there and
/// invokes the caller's callback — keeping that callback free to hold non-`Send` GTK state.
///
/// `new()` returns at once: the connection is made on the watcher's thread, which may take
/// seconds, and `new()` runs on the main thread while the window is built. With no audio server to
/// reach (sandboxes, CI) the thread logs that headphone support is off and ends. Once the first
/// connection succeeded, the thread lives as long as the process: a lost connection is logged and
/// re-established with backoff (see `RECONNECT_BACKOFF`), never silently given up on.
pub struct PulseRouteWatcher {
    sender: EventSender,
    /// Set by `Drop`, checked by the watcher thread once per `iterate()` return — so a watcher
    /// dropped when the shell that built it is torn down (sign-out, switching server/account:
    /// see `app/src/application.rs`'s `show_main`/`show_main_or_welcome`) doesn't keep its
    /// libpulse connection and OS thread running forever. This only takes effect on the *next*
    /// audio-server event, since `iterate(true)` blocks until one arrives; `Drop` doesn't wait
    /// for that — see its own doc comment for the part that matters immediately.
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

type EventSender = std::sync::Arc<std::sync::Mutex<Option<futures::channel::mpsc::UnboundedSender<RouteEvent>>>>;

/// Backoff between reconnection attempts after an established connection was lost: starts at
/// the first value, doubles per failure up to the second, resets once a connection is Ready.
const RECONNECT_BACKOFF: (std::time::Duration, std::time::Duration) = (std::time::Duration::from_secs(1), std::time::Duration::from_secs(30));

enum Signal {
    StateChanged,
    /// A sink/card/server changed — re-read the sink list. Card events matter because port
    /// availability is a *card* property in PulseAudio's model: a jack event always posts a card
    /// change, and the sink change that (current) servers post alongside it is a courtesy.
    SinkListChanged,
    SinkRemoved(u32),
    /// One complete pass over the sink list.
    Scan(Vec<SinkReading>),
    SubscribeFailed,
}

/// Why a connection attempt ended (it never ends on its own while healthy).
enum ConnectionEnd {
    /// Somebody asked the main loop to quit — leave for good.
    Quit,
    Failed(String),
}

/// Names the headphone-ish ports of a sink. Port names are PulseAudio conventions like
/// `analog-output-headphones` (plain ALSA paths), `[Out] Headphones` (ALSA UCM, i.e. the Librem 5
/// and PinePhone) or `headphone-output` / `headset-output` (Bluetooth); the match is
/// case-insensitive to be robust across drivers.
fn headphone_availability(sink_ports: &[libpulse_binding::context::introspect::SinkPortInfo<'_>]) -> Option<PortAvailability> {
    sink_ports
        .iter()
        .find(|port| {
            let name = port.name.as_deref().unwrap_or("").to_ascii_lowercase();
            name.contains("headphone") || name.contains("headset")
        })
        .map(|port| match port.available {
            libpulse_binding::def::PortAvailable::Yes => PortAvailability::Yes,
            libpulse_binding::def::PortAvailable::No => PortAvailability::No,
            _ => PortAvailability::Unknown,
        })
}

impl PulseRouteWatcher {
    pub fn new() -> Result<Self, RouteWatchError> {
        let sender: EventSender = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), RouteWatchError>>();

        let sender_for_thread = sender.clone();
        let stop_for_thread = stop.clone();
        std::thread::Builder::new()
            .name("abs-route-watch".into())
            .spawn(move || {
                // Consumed by the first attempt's outcome; later attempts only log.
                let mut init_tx = Some(init_tx);
                let mut backoff = RECONNECT_BACKOFF.0;
                loop {
                    if stop_for_thread.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    match watch_connection(&mut init_tx, &sender_for_thread, &mut backoff, &stop_for_thread) {
                        ConnectionEnd::Quit => return,
                        ConnectionEnd::Failed(reason) => {
                            if init_tx.take().is_some() {
                                // The very first connection never got going: the feature is off
                                // for this run — nothing to retry into. `new()` has returned long
                                // since (it never waits for the connection), so this is where it
                                // is said.
                                tracing::warn!(%reason, "couldn't watch the audio server for headphone changes; unplug-pause will be unavailable");
                                return;
                            }
                            tracing::warn!(%reason, retry_in_secs = backoff.as_secs(), "lost the audio-server connection; headphone watching will resume after reconnecting");
                            std::thread::sleep(backoff);
                            backoff = (backoff * 2).min(RECONNECT_BACKOFF.1);
                        }
                    }
                }
            })
            .map_err(|err| RouteWatchError::NoAudioServer(format!("couldn't spawn the watcher thread: {err}")))?;

        // Not waited for: connecting to the audio server can take seconds (or fail after a
        // timeout), and `new()` runs on the main thread while the window is being built. Events
        // flow once the thread's connection is up; before that there is nothing to miss, since no
        // player has started. A first connection that fails is logged by the thread.
        drop(init_rx);
        Ok(Self { sender, stop })
    }
}

impl Drop for PulseRouteWatcher {
    fn drop(&mut self) {
        // Closing the channel (clearing the sender) stops event delivery immediately: it ends
        // the `rx.next()` loop `start()` spawned onto the GLib main context, which otherwise
        // has no way to notice this watcher went away and would keep calling `start()`'s
        // callback — and with it, whatever `PlayerController` that callback closed over — for as
        // long as the process runs. That closure holding the controller alive is what actually
        // matters here: the OS thread and its libpulse connection are lower stakes (no more
        // events reach anyone once the sender is cleared) and are asked to stop via `stop`, but
        // only notice on the next audio-server event, since `iterate(true)` blocks until one.
        *self.sender.lock().expect("route-watch sender mutex") = None;
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// One connection's lifetime: connect, subscribe, scan, classify until the connection ends.
/// `init_tx` is taken (and told `Ok`) the moment the connection is Ready; `backoff` is reset
/// at the same moment. The classifier is fresh per connection: after a reconnect nothing is
/// known, so the first scan reports the state of the world the same way startup does.
fn watch_connection(
    init_tx: &mut Option<std::sync::mpsc::Sender<Result<(), RouteWatchError>>>,
    sender: &EventSender,
    backoff: &mut std::time::Duration,
    stop: &std::sync::atomic::AtomicBool,
) -> ConnectionEnd {
    use libpulse_binding::callbacks::ListResult;
    use libpulse_binding::context::subscribe::{Facility, InterestMaskSet, Operation};
    use libpulse_binding::context::{Context, FlagSet, State};
    use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};

    let Some(mut mainloop) = Mainloop::new() else {
        return ConnectionEnd::Failed("couldn't create the PulseAudio main loop".into());
    };
    let Some(mut context) = Context::new(&mainloop, "audiobooklet-route-watch") else {
        return ConnectionEnd::Failed("couldn't create the PulseAudio context".into());
    };

    // Everything the libpulse callbacks do is "send a Signal and get out" — a callback running
    // inside `iterate()` must never re-enter the `Context` that owns it. Classification and
    // introspection both happen in this thread's own loop, strictly between `iterate()` calls.
    let (signal_tx, signal_rx) = std::sync::mpsc::channel::<Signal>();
    context.set_state_callback(Some(Box::new({
        let signal_tx = signal_tx.clone();
        move || {
            let _ = signal_tx.send(Signal::StateChanged);
        }
    })));
    if context.connect(None, FlagSet::NOFLAGS, None).is_err() {
        return ConnectionEnd::Failed("connecting to the audio server failed".into());
    }
    context.set_subscribe_callback(Some(Box::new({
        let signal_tx = signal_tx.clone();
        move |facility, operation, index| {
            let _ = signal_tx.send(match (facility, operation) {
                (Some(Facility::Sink), Some(Operation::Removed)) => Signal::SinkRemoved(index),
                // Sink new/changed, card changes (jack events) and server changes (default-sink
                // switch) all mean "re-read the sinks' ports".
                (Some(Facility::Sink | Facility::Card | Facility::Server), _) => Signal::SinkListChanged,
                _ => return,
            });
        }
    })));

    let mut classifier = RouteClassifier::default();
    let mut subscribed = false;
    loop {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) => return ConnectionEnd::Quit,
            IterateResult::Err(err) => return ConnectionEnd::Failed(format!("the audio-server connection failed: {err}")),
        }
        // Checked once per audio-server event (see `PulseRouteWatcher::stop`'s doc comment for
        // why that's good enough — the channel close on `Drop` is what stops event delivery
        // immediately; this just eventually releases the connection and this thread).
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return ConnectionEnd::Quit;
        }

        let mut rescan = false;
        while let Ok(signal) = signal_rx.try_recv() {
            match signal {
                Signal::StateChanged => match context.get_state() {
                    State::Ready => {
                        if !subscribed {
                            // The `Operation` this returns is just the subscription request's
                            // own ack — dropped here (dropping doesn't cancel it); the ack
                            // callback is what reports failure.
                            context.subscribe(InterestMaskSet::SINK | InterestMaskSet::CARD | InterestMaskSet::SERVER, {
                                let signal_tx = signal_tx.clone();
                                move |success| {
                                    if !success {
                                        let _ = signal_tx.send(Signal::SubscribeFailed);
                                    }
                                }
                            });
                            subscribed = true;
                            *backoff = RECONNECT_BACKOFF.0;
                            tracing::info!("watching the audio server for headphone changes");
                            // The first successful scan completes initialization. Nothing is
                            // emitted for it beyond what the classifier's `None` transitions
                            // allow (see its docs).
                            if let Some(tx) = init_tx.take() {
                                let _ = tx.send(Ok(()));
                            }
                            rescan = true;
                        }
                    }
                    // No audio server at all (empty sandbox), or the server died.
                    State::Failed | State::Terminated => return ConnectionEnd::Failed("the audio-server connection failed".into()),
                    _ => {}
                },
                Signal::SinkListChanged => rescan = true,
                Signal::SinkRemoved(index) => {
                    tracing::debug!(sink = index, "sink removed");
                    if let Some(event) = classifier.on_sink_removed(index) {
                        dispatch(sender, event);
                    }
                }
                Signal::Scan(readings) => {
                    tracing::debug!(?readings, "scanned sinks for headphone ports");
                    if let Some(event) = classifier.on_scan(&readings) {
                        dispatch(sender, event);
                    }
                }
                Signal::SubscribeFailed => return ConnectionEnd::Failed("subscribing to sink events failed".into()),
            }
        }

        if rescan {
            // Accumulate the whole list and hand it over at `End` as one snapshot: the classifier
            // needs to know which sinks are *gone*, which single items can't tell it.
            context.introspect().get_sink_info_list({
                let signal_tx = signal_tx.clone();
                let mut readings = Vec::new();
                move |result| match result {
                    ListResult::Item(info) => readings.push(SinkReading { index: info.index, headphones: headphone_availability(&info.ports) }),
                    ListResult::End => {
                        let _ = signal_tx.send(Signal::Scan(std::mem::take(&mut readings)));
                    }
                    // A failed listing is not a snapshot; the next event triggers another.
                    ListResult::Error => readings.clear(),
                }
            });
        }
    }
}

fn dispatch(sender: &EventSender, event: RouteEvent) {
    tracing::info!(?event, "headphone route changed");
    if let Some(tx) = sender.lock().expect("route-watch sender mutex").as_ref() {
        let _ = tx.unbounded_send(event);
    }
}

impl RouteWatcher for PulseRouteWatcher {
    fn start(&mut self, on_event: Box<dyn Fn(RouteEvent)>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<RouteEvent>();
        *self.sender.lock().expect("route-watch sender mutex") = Some(tx);
        // Runs on the GLib main loop (this is always called from it), so the callback may capture
        // non-`Send` GTK state freely — only `RouteEvent`s themselves cross the thread boundary.
        glib::spawn_future_local(async move {
            use futures::stream::StreamExt;
            while let Some(event) = rx.next().await {
                on_event(event);
            }
        });
    }
}

#[cfg(test)]
type FakeCallbackCell = std::cell::RefCell<Option<Box<dyn Fn(RouteEvent)>>>;

#[cfg(test)]
pub struct FakeRouteWatcher {
    callback: FakeCallbackCell,
}

#[cfg(test)]
impl FakeRouteWatcher {
    pub fn new() -> std::rc::Rc<Self> {
        std::rc::Rc::new(Self { callback: FakeCallbackCell::new(None) })
    }

    pub fn simulate(&self, event: RouteEvent) {
        if let Some(callback) = self.callback.borrow().as_ref() {
            callback(event);
        }
    }
}

#[cfg(test)]
impl RouteWatcher for FakeRouteWatcher {
    fn start(&mut self, on_event: Box<dyn Fn(RouteEvent)>) {
        *self.callback.borrow_mut() = Some(on_event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-sink world (the plain Librem 5 / laptop case): sink 1 with a headphone port.
    fn wired(available: PortAvailability) -> Vec<SinkReading> {
        vec![SinkReading { index: 1, headphones: Some(available) }]
    }

    #[test]
    fn classifier_silent_at_startup_whatever_the_headphone_state_is() {
        let mut classifier = RouteClassifier::default();
        assert_eq!(classifier.on_scan(&wired(PortAvailability::No)), None, "startup with headphones unplugged is the state of the world, not an event");

        // A fresh process with headphones already plugged in: `None -> yes` does emit Replugged
        // (deliberately — it's what a Bluetooth reconnection looks like after the sink vanished
        // and wiped the state), which is inert at true startup because nothing had been
        // auto-paused for it to undo. Assert the event to pin the design.
        let mut classifier = RouteClassifier::default();
        assert_eq!(classifier.on_scan(&wired(PortAvailability::Yes)), Some(RouteEvent::Replugged));
    }

    #[test]
    fn classifier_reports_unplug_on_yes_to_no() {
        let mut classifier = RouteClassifier::default();
        classifier.on_scan(&wired(PortAvailability::Yes));
        assert_eq!(classifier.on_scan(&wired(PortAvailability::No)), Some(RouteEvent::Unplugged));
        // And it doesn't repeat itself: still absent is not a new event.
        assert_eq!(classifier.on_scan(&wired(PortAvailability::No)), None);
    }

    #[test]
    fn classifier_reports_replug_on_no_to_yes() {
        let mut classifier = RouteClassifier::default();
        classifier.on_scan(&wired(PortAvailability::Yes));
        classifier.on_scan(&wired(PortAvailability::No));
        assert_eq!(classifier.on_scan(&wired(PortAvailability::Yes)), Some(RouteEvent::Replugged));
    }

    #[test]
    fn classifier_treats_a_bt_reconnection_as_replug() {
        // Bluetooth: the sink disappears (wiping state) and returns later with its port available.
        let mut classifier = RouteClassifier::default();
        classifier.on_scan(&wired(PortAvailability::Yes));
        assert_eq!(classifier.on_sink_removed(1), Some(RouteEvent::Unplugged));
        assert_eq!(classifier.on_scan(&wired(PortAvailability::Yes)), Some(RouteEvent::Replugged));
    }

    #[test]
    fn classifier_ignores_unknown_availability_and_missing_ports() {
        let mut classifier = RouteClassifier::default();
        classifier.on_scan(&wired(PortAvailability::Yes));
        // A USB DAC that can't do jack detection: no event, no state change.
        assert_eq!(classifier.on_scan(&wired(PortAvailability::Unknown)), None);
        assert_eq!(classifier.on_scan(&wired(PortAvailability::No)), Some(RouteEvent::Unplugged), "the last *known* state is what the unplug is measured against");
        // A speaker-only sink (no headphone port at all): ignored.
        assert_eq!(classifier.on_scan(&[SinkReading { index: 1, headphones: None }]), None);
        // And a sink that only ever reported Unknown never enters the picture.
        let mut classifier = RouteClassifier::default();
        assert_eq!(classifier.on_scan(&wired(PortAvailability::Unknown)), None);
        assert_eq!(classifier.on_sink_removed(1), None);
    }

    #[test]
    fn classifier_ignores_sink_removal_when_headphones_were_not_in_use() {
        let mut classifier = RouteClassifier::default();
        // Some other output (speakers, HDMI) going away, or removals before anything was observed.
        assert_eq!(classifier.on_sink_removed(7), None);
        classifier.on_scan(&wired(PortAvailability::No));
        assert_eq!(classifier.on_sink_removed(7), None);
        assert_eq!(classifier.on_sink_removed(1), None);
    }

    /// The bug this per-sink design fixes: a Bluetooth sink's `headphone-output` port (available
    /// while streaming) next to the wired card's unplugged headphone port used to feed one shared
    /// state `yes, no, yes, no…` on every rescan, pausing playback on any sink event at all.
    #[test]
    fn classifier_does_not_flip_flop_across_a_wired_and_a_bluetooth_sink() {
        let both = [
            SinkReading { index: 1, headphones: Some(PortAvailability::No) },
            SinkReading { index: 2, headphones: Some(PortAvailability::Yes) },
        ];
        let mut classifier = RouteClassifier::default();
        assert_eq!(classifier.on_scan(&both), Some(RouteEvent::Replugged), "the Bluetooth headphones are present");
        assert_eq!(classifier.on_scan(&both), None, "a rescan with nothing changed is not an event");
        assert_eq!(classifier.on_scan(&both), None);
        // Same readings in the other order are the same world.
        assert_eq!(classifier.on_scan(&[both[1], both[0]]), None);
    }

    #[test]
    fn classifier_unplugs_only_when_the_last_headphone_output_is_gone() {
        let mut classifier = RouteClassifier::default();
        classifier.on_scan(&[
            SinkReading { index: 1, headphones: Some(PortAvailability::Yes) },
            SinkReading { index: 2, headphones: Some(PortAvailability::Yes) },
        ]);
        // Wired jack pulled while the Bluetooth set keeps streaming: the listener still hears it.
        assert_eq!(
            classifier.on_scan(&[
                SinkReading { index: 1, headphones: Some(PortAvailability::No) },
                SinkReading { index: 2, headphones: Some(PortAvailability::Yes) },
            ]),
            None
        );
        // Bluetooth goes away too: now the output is really gone.
        assert_eq!(classifier.on_sink_removed(2), Some(RouteEvent::Unplugged));
        // A scan without the removed sink stays quiet; plugging the wire back in resumes.
        assert_eq!(classifier.on_scan(&wired(PortAvailability::No)), None);
        assert_eq!(classifier.on_scan(&wired(PortAvailability::Yes)), Some(RouteEvent::Replugged));
    }

    #[test]
    fn classifier_forgets_sinks_that_vanish_between_scans() {
        // A Bluetooth sink may simply be missing from the next scan (removal event lost or
        // coalesced): its absence must count the same as its removal event.
        let mut classifier = RouteClassifier::default();
        classifier.on_scan(&[SinkReading { index: 2, headphones: Some(PortAvailability::Yes) }]);
        assert_eq!(classifier.on_scan(&[]), Some(RouteEvent::Unplugged));
        assert_eq!(classifier.on_scan(&[]), None);
    }

    #[test]
    fn headphone_availability_finds_headphone_ports_and_ignores_others() {
        fn make_port<'a>(name: Option<&'a str>, available: PortAvailability) -> libpulse_binding::context::introspect::SinkPortInfo<'a> {
            libpulse_binding::context::introspect::SinkPortInfo {
                name: name.map(Into::into),
                description: None,
                priority: 0,
                available: match available {
                    PortAvailability::Unknown => libpulse_binding::def::PortAvailable::Unknown,
                    PortAvailability::Yes => libpulse_binding::def::PortAvailable::Yes,
                    PortAvailability::No => libpulse_binding::def::PortAvailable::No,
                },
            }
        }

        let ports = vec![
            make_port(Some("analog-output-speaker"), PortAvailability::Yes),
            make_port(Some("analog-output-headphones"), PortAvailability::No),
        ];
        assert_eq!(headphone_availability(&ports), Some(PortAvailability::No), "the headphone port is the one that counts, not the first port");

        let speakers_only = vec![make_port(Some("analog-output-speaker"), PortAvailability::Yes)];
        assert_eq!(headphone_availability(&speakers_only), None);
    }

    /// Drains the default `MainContext` — the same one `glib::spawn_future_local` schedules
    /// onto — until `done()` returns true or `timeout` elapses. `iteration(false)` is
    /// non-blocking, so this is a plain poll loop, not a nested main loop (same shape as
    /// `app`'s own `test_support::pump_until`, which this crate has no dependency on).
    fn pump_until(done: impl Fn() -> bool, timeout: std::time::Duration) {
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + timeout;
        while !done() && std::time::Instant::now() < deadline {
            while context.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Builds a `PulseRouteWatcher` around an unconnected sender/stop pair — same struct the real
    /// `new()` produces, minus the libpulse connection — so `start()`/`Drop` can be exercised by
    /// feeding `dispatch()` (what the watcher thread actually calls from inside libpulse
    /// callbacks) directly.
    fn test_watcher() -> PulseRouteWatcher {
        PulseRouteWatcher { sender: std::sync::Arc::new(std::sync::Mutex::new(None)), stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) }
    }

    /// Both scenarios below touch `glib::MainContext::default()`, which — unlike a `Mainloop`
    /// this module builds itself — is a single **process-wide** singleton, not one per thread.
    /// Rust's test harness gives every `#[test]` fn its own OS thread (true even under
    /// `--test-threads=1`, which only limits concurrency, not which thread each runs on; `app`'s
    /// own `gtk_fast_scenarios` driver exists for the same reason). A `!Send` future
    /// `spawn_future_local` attaches to that shared context can only be touched from the thread
    /// that created it — two separate `#[test]` fns each spawning one onto it crashes the process
    /// ("non-unwinding panic" out of glib's `ThreadGuard`) the moment either context iteration
    /// touches the other's leftover source. One `#[test]` fn running both scenarios in sequence,
    /// on the one thread it owns, sidesteps that — this crate has nothing else touching the
    /// default context, so nothing else can collide with it either.
    #[test]
    fn route_watcher_delivery() {
        // `dispatch()` (what the watcher thread calls from inside libpulse callbacks) crosses a
        // real OS thread boundary into the `futures` channel `start()` set up, and the assertion
        // only passes if `start()`'s `glib::spawn_future_local` task actually receives it on the
        // default `MainContext` and invokes the callback. This is the link the classifier's unit
        // tests (all synchronous, no thread, no main loop) don't cover, and the one most likely
        // to go silently wrong on a real device: the watcher thread logs "headphone route
        // changed" from inside `dispatch()` regardless of whether anything downstream ever
        // receives it.
        {
            let mut watcher = test_watcher();
            let received: std::rc::Rc<std::cell::RefCell<Vec<RouteEvent>>> = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            watcher.start(Box::new({
                let received = received.clone();
                move |event| received.borrow_mut().push(event)
            }));

            // A real background thread, not just a same-thread call — this is what would catch
            // e.g. `spawn_future_local` requiring a context this test's thread never acquired.
            let sender = watcher.sender.clone();
            std::thread::spawn(move || dispatch(&sender, RouteEvent::Unplugged));

            pump_until(|| !received.borrow().is_empty(), std::time::Duration::from_secs(5));
            assert_eq!(*received.borrow(), vec![RouteEvent::Unplugged], "an event dispatched from another thread must reach start()'s callback on the main context");
        }

        // `Drop`'s whole point: without it, `start()`'s callback — and whatever it closed over (a
        // `PlayerController`, on a real device) — would go on receiving events forever after the
        // watcher itself was dropped (a shell rebuild: sign-out, switching server/account).
        {
            let mut watcher = test_watcher();
            let received: std::rc::Rc<std::cell::RefCell<Vec<RouteEvent>>> = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            watcher.start(Box::new({
                let received = received.clone();
                move |event| received.borrow_mut().push(event)
            }));
            let sender = watcher.sender.clone();

            drop(watcher);
            std::thread::spawn(move || dispatch(&sender, RouteEvent::Unplugged));
            // Give the (dead) callback every chance to run before asserting it didn't.
            pump_until(|| false, std::time::Duration::from_millis(200));

            assert!(received.borrow().is_empty(), "an event dispatched after the watcher was dropped must not reach the old callback");
        }
    }

    #[test]
    fn new_returns_at_once_even_when_the_audio_server_is_unreachable() {
        // The environment is process-wide, but no other test in this crate reads PULSE_SERVER.
        std::env::set_var("PULSE_SERVER", "unix:/nonexistent/abs-test-pulse");
        let started = std::time::Instant::now();
        let watcher = PulseRouteWatcher::new().expect("the thread starts; the missing server is logged there");
        assert!(started.elapsed() < std::time::Duration::from_millis(250), "new() waited {:?}", started.elapsed());
        drop(watcher);
    }

    /// Registers against a *real* audio server — only checkable when one is reachable. This
    /// sandbox has no PulseAudio/PipeWire socket at all, so the watcher's thread logs that the
    /// feature is off rather than panicking. Run explicitly via `cargo test -p abs-player -- --ignored route_watch::tests::new`
    /// once a running audio server is confirmed present.
    #[test]
    #[ignore]
    fn new_connects_and_initializes_against_a_real_audio_server() {
        let mut watcher = PulseRouteWatcher::new().expect("connect to the audio server");
        watcher.start(Box::new(|_event| {}));
    }
}
