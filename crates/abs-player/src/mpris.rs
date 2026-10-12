//! MPRIS2 (`org.mpris.MediaPlayer2`) system media integration —
//! `docs/design/ui-spec.md`'s "System media integration" section. This lives in `abs-player`
//! rather than `app` because it's real OS I/O (a D-Bus session), the same category this crate
//! already owns for GStreamer — not a GTK widget-toolkit concern. `gio`/`glib` are direct
//! dependencies here (already present transitively via `gtk4`/`adw`), not `zbus`: no new external
//! crate, and object registration is exactly `gio::DBusConnection`'s own API.
//!
//! This module never imports anything from `app` — [`register`] takes a plain trait object
//! (`Rc<dyn MprisCommands>`) supplied by the caller and calls back into it; `app/src/player.rs`
//! is the only place that ties `MprisCommands` to a real `PlayerController`. That's the whole
//! dependency-direction story: `app` depends on `abs-player`, never the reverse.
//!
//! `Next`/`Previous` go to the next/previous chapter, never a literal track change (this app has
//! no playlist concept) — the system media card already has its own seek buttons for skipping.
//! A book without chapters falls back to skip-forward/back-N-seconds, per
//! `docs/design/ui-spec.md`'s mapping. Which of the two happens is the caller's business.

use std::cell::RefCell;
use std::rc::Rc;

use gio::prelude::*;

const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const MEDIA_PLAYER2_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// A fixed dummy track id — this app has no MPRIS `TrackList` support (no playlist concept), so
/// there's nothing meaningful to distinguish one track's id from another's. Real MPRIS clients
/// (lock-screen widgets, GNOME Shell's media controls) only use this as an opaque identifier, not
/// as a lookup key, so a fixed value is spec-compliant even though it's the same for every item.
const TRACK_ID_PATH: &str = "/org/mpris/MediaPlayer2/Track/1";

const INTROSPECTION_XML: &str = r#"
<node>
  <interface name="org.mpris.MediaPlayer2">
    <method name="Raise"/>
    <method name="Quit"/>
    <property name="CanQuit" type="b" access="read"/>
    <property name="CanRaise" type="b" access="read"/>
    <property name="HasTrackList" type="b" access="read"/>
    <property name="Identity" type="s" access="read"/>
    <property name="SupportedUriSchemes" type="as" access="read"/>
    <property name="SupportedMimeTypes" type="as" access="read"/>
  </interface>
  <interface name="org.mpris.MediaPlayer2.Player">
    <method name="Next"/>
    <method name="Previous"/>
    <method name="Pause"/>
    <method name="PlayPause"/>
    <method name="Stop"/>
    <method name="Play"/>
    <method name="Seek">
      <arg direction="in" name="Offset" type="x"/>
    </method>
    <method name="SetPosition">
      <arg direction="in" name="TrackId" type="o"/>
      <arg direction="in" name="Position" type="x"/>
    </method>
    <signal name="Seeked">
      <arg name="Position" type="x"/>
    </signal>
    <property name="PlaybackStatus" type="s" access="read"/>
    <property name="Rate" type="d" access="read"/>
    <property name="Metadata" type="a{sv}" access="read"/>
    <property name="Position" type="x" access="read"/>
    <property name="MinimumRate" type="d" access="read"/>
    <property name="MaximumRate" type="d" access="read"/>
    <property name="CanGoNext" type="b" access="read"/>
    <property name="CanGoPrevious" type="b" access="read"/>
    <property name="CanPlay" type="b" access="read"/>
    <property name="CanPause" type="b" access="read"/>
    <property name="CanSeek" type="b" access="read"/>
    <property name="CanControl" type="b" access="read"/>
  </interface>
</node>
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

impl PlaybackStatus {
    fn as_str(self) -> &'static str {
        match self {
            PlaybackStatus::Playing => "Playing",
            PlaybackStatus::Paused => "Paused",
            PlaybackStatus::Stopped => "Stopped",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: Option<String>,
    pub length_micros: i64,
    /// A `file://` URI — MPRIS's `mpris:artUrl` accepts any URI, and the cover is always a local
    /// cache file, never served directly from a remote URL.
    pub art_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PlayerState {
    pub status: PlaybackStatus,
    pub metadata: TrackMetadata,
    pub position_micros: i64,
    pub rate: f64,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self { status: PlaybackStatus::Stopped, metadata: TrackMetadata::default(), position_micros: 0, rate: 1.0 }
    }
}

/// Commands an MPRIS client (lock screen, GNOME Shell's media widget, a Bluetooth headset) can
/// invoke. Implemented by `app/src/player.rs`'s bridge over the real `PlayerController` — this
/// trait is the whole boundary, so `abs-player` never needs to know what a `PlayerController` is.
pub trait MprisCommands {
    fn play_pause(&self);
    fn play(&self);
    fn pause(&self);
    /// A relative seek, in microseconds (matches MPRIS's `Seek` method exactly) — positive skips
    /// forward, negative skips back.
    fn seek(&self, offset_micros: i64);
    /// An absolute seek, in microseconds (matches MPRIS's `SetPosition` method).
    fn set_position(&self, position_micros: i64);
    /// Mapped to skip-forward-N-seconds, never a literal track change (see module docs).
    fn next(&self);
    /// Mapped to skip-back-N-seconds, never a literal track change (see module docs).
    fn previous(&self);
}

#[derive(Debug, thiserror::Error)]
pub enum MprisError {
    #[error("couldn't reach the D-Bus session bus: {0}")]
    NoSessionBus(glib::Error),
    #[error("couldn't parse MPRIS introspection XML: {0}")]
    InvalidIntrospection(glib::Error),
    #[error("MPRIS introspection XML is missing the {0} interface")]
    MissingInterface(&'static str),
    #[error("couldn't register the MPRIS object: {0}")]
    RegistrationFailed(glib::Error),
}

/// Registers this app as an MPRIS2 media player on the session bus. Returns `Err` rather than
/// panicking if no session bus is reachable (headless CI, a sandboxed environment with no D-Bus) —
/// callers should log a warning and continue; MPRIS absence must never be fatal to playback.
/// `bus_name` becomes `org.mpris.MediaPlayer2.{bus_name}` (it must match what a sandbox may own);
/// `identity` is the human-readable name media widgets show.
pub fn register(bus_name: &str, identity: &str, commands: Rc<dyn MprisCommands>) -> Result<MprisHandle, MprisError> {
    let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).map_err(MprisError::NoSessionBus)?;

    let node_info = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML).map_err(MprisError::InvalidIntrospection)?;
    let media_player2_info = node_info.lookup_interface(MEDIA_PLAYER2_IFACE).ok_or(MprisError::MissingInterface(MEDIA_PLAYER2_IFACE))?;
    let player_info = node_info.lookup_interface(PLAYER_IFACE).ok_or(MprisError::MissingInterface(PLAYER_IFACE))?;

    let state = Rc::new(RefCell::new(PlayerState::default()));
    let identity = identity.to_string();

    let media_player2_registration = connection
        .register_object(OBJECT_PATH, &media_player2_info)
        .method_call(|_conn, _sender, _path, _iface, method, _params, invocation| {
            // `Raise`/`Quit` have no meaningful behavior for a phone player with no separate
            // "bring to front" concept distinct from just switching apps — acknowledge, no-op.
            let _ = method;
            invocation.return_value(None);
        })
        .property({
            let identity = identity.clone();
            move |_conn, _sender, _path, _iface, property| media_player2_property(property, &identity)
        })
        .build()
        .map_err(MprisError::RegistrationFailed)?;

    let player_registration = match connection
        .register_object(OBJECT_PATH, &player_info)
        .method_call({
            let commands = commands.clone();
            let state = state.clone();
            move |_conn, sender, _path, _iface, method, params, invocation| {
                // One chokepoint for every inbound Player-interface call (the desktop shell's
                // media widget, media keys — including the spurious headset-button press a TRRS
                // unplug generates — a Bluetooth AVRCP peer, `playerctl`, …). Without it, an
                // external `PlayPause` resuming playback left no trace in the log at all. The
                // sender is the caller's unique bus name; `busctl --user status <name>` maps it
                // to a process.
                tracing::info!(%method, sender = sender.unwrap_or("?"), "MPRIS command received");
                match handle_player_call(method, &params, commands.as_ref(), &state) {
                    Ok(reply) => invocation.return_value(reply.as_ref()),
                    Err(err) => invocation.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", &err.to_string()),
                }
            }
        })
        .property({
            let state = state.clone();
            move |_conn, _sender, _path, _iface, property| player_property(property, &state.borrow())
        })
        .build()
    {
        Ok(registration) => registration,
        Err(err) => {
            // `RegistrationId` has no drop guard: leaving the first object exported would make
            // every later `register` fail on it.
            if let Err(unregister_err) = connection.unregister_object(media_player2_registration) {
                tracing::warn!(err = %unregister_err, "couldn't roll back the first MPRIS object after the second failed");
            }
            return Err(MprisError::RegistrationFailed(err));
        }
    };

    // Owning the well-known name is best-effort: a failure here (e.g. the name is already taken
    // by another instance of this app) shouldn't tear down the object registrations above —
    // clients that already know the object path can still reach it directly.
    let owner_id = gio::bus_own_name_on_connection(
        &connection,
        &format!("org.mpris.MediaPlayer2.{bus_name}"),
        gio::BusNameOwnerFlags::NONE,
        |_conn, _name| {},
        |_conn, name| tracing::warn!(name, "couldn't own the MPRIS well-known bus name"),
    );

    Ok(MprisHandle {
        connection,
        state,
        media_player2_registration: Some(media_player2_registration),
        player_registration: Some(player_registration),
        owner_id: Some(owner_id),
    })
}

/// Dropping the handle takes the app off the bus again: both objects are unregistered and the
/// well-known name released, so media keys and the lock-screen card stop reaching a player that
/// has been replaced (an account switch builds a new one).
pub struct MprisHandle {
    connection: gio::DBusConnection,
    state: Rc<RefCell<PlayerState>>,
    media_player2_registration: Option<gio::RegistrationId>,
    player_registration: Option<gio::RegistrationId>,
    owner_id: Option<gio::OwnerId>,
}

impl Drop for MprisHandle {
    fn drop(&mut self) {
        for registration in [self.media_player2_registration.take(), self.player_registration.take()].into_iter().flatten() {
            if let Err(err) = self.connection.unregister_object(registration) {
                tracing::warn!(%err, "couldn't unregister an MPRIS object");
            }
        }
        if let Some(owner_id) = self.owner_id.take() {
            gio::bus_unown_name(owner_id);
        }
    }
}

impl MprisHandle {
    /// Replaces the whole player state and notifies any listening client via the standard
    /// `org.freedesktop.DBus.Properties.PropertiesChanged` signal — this is what keeps a
    /// lock-screen media card in sync with real playback.
    pub fn update(&self, new_state: PlayerState) {
        let previous = self.state.borrow().clone();
        *self.state.borrow_mut() = new_state.clone();

        // `Position` is never signalled by `PropertiesChanged` (below); clients learn of a jump
        // — a seek, a chapter tap, a resume at the saved place — from `Seeked`. Playing moves it
        // by well under `SEEKED_MIN_JUMP_MICROS` between updates, so a bigger step is a seek.
        if position_jumped(&previous, &new_state) {
            if let Err(err) = self.connection.emit_signal(None, OBJECT_PATH, PLAYER_IFACE, "Seeked", Some(&glib::Variant::tuple_from_iter([new_state.position_micros.to_variant()]))) {
                tracing::warn!(%err, "couldn't emit MPRIS Seeked");
            }
        }

        if !player_state_changed(&previous, &new_state) {
            return;
        }

        let params = glib::Variant::tuple_from_iter([
            PLAYER_IFACE.to_variant(),
            changed_properties(&new_state),
            Vec::<String>::new().to_variant(),
        ]);
        if let Err(err) = self.connection.emit_signal(None, OBJECT_PATH, PROPERTIES_IFACE, "PropertiesChanged", Some(&params)) {
            tracing::warn!(%err, "couldn't emit MPRIS PropertiesChanged");
        }
    }
}

/// Between two updates (one per 250 ms tick, at up to 3x) playback moves the position by at most
/// about a second; a larger step is a seek.
const SEEKED_MIN_JUMP_MICROS: i64 = 2_000_000;

fn position_jumped(previous: &PlayerState, new: &PlayerState) -> bool {
    (new.position_micros - previous.position_micros).abs() > SEEKED_MIN_JUMP_MICROS
}

/// Whether any MPRIS-relevant field changed. `position_micros` is deliberately excluded —
/// see the comment in `MprisHandle::update` on why `Position` never triggers a signal.
fn player_state_changed(previous: &PlayerState, new: &PlayerState) -> bool {
    previous.status != new.status || previous.metadata != new.metadata || previous.rate != new.rate
}

/// Every Player property whose value can change, sent in full with each `PropertiesChanged`.
/// A client (Phosh's media widget, GNOME Shell's) reads all properties once when the app appears
/// on the bus — with nothing loaded, so the `Can*` ones are `false` — and from then on only
/// learns new values from this signal; a property missing here stays at its first value in the
/// client forever. The `Can*` values depend only on `status`, which `player_state_changed`
/// compares, so they never change without a signal going out.
///
/// `Position` is deliberately left out, per the MPRIS spec itself ("Position ... may be changed
/// without notification") — clients poll it, or learn of a jump from `Seeked`. `CanControl` and
/// the rate bounds never change.
const SIGNALLED_PROPERTIES: [&str; 8] = ["PlaybackStatus", "Metadata", "Rate", "CanGoNext", "CanGoPrevious", "CanPlay", "CanPause", "CanSeek"];

/// The `changed_properties` dict of a `PropertiesChanged` signal, built from the same getter
/// clients' `Get`/`GetAll` calls use, so the two can never disagree.
fn changed_properties(state: &PlayerState) -> glib::Variant {
    let changed = glib::VariantDict::new(None);
    for property in SIGNALLED_PROPERTIES {
        changed.insert_value(property, &player_property(property, state));
    }
    changed.end()
}

fn media_player2_property(property: &str, identity: &str) -> glib::Variant {
    match property {
        "CanQuit" => false.to_variant(),
        "CanRaise" => false.to_variant(),
        "HasTrackList" => false.to_variant(),
        "Identity" => identity.to_variant(),
        "SupportedUriSchemes" => Vec::<String>::new().to_variant(),
        "SupportedMimeTypes" => Vec::<String>::new().to_variant(),
        _ => false.to_variant(),
    }
}

fn player_property(property: &str, state: &PlayerState) -> glib::Variant {
    match property {
        "PlaybackStatus" => state.status.as_str().to_variant(),
        "Rate" => state.rate.to_variant(),
        "Metadata" => metadata_variant(&state.metadata),
        "Position" => state.position_micros.to_variant(),
        "MinimumRate" => 0.8_f64.to_variant(),
        "MaximumRate" => 3.0_f64.to_variant(),
        // Nothing loaded: there is nothing to play, pause, skip or seek in.
        "CanGoNext" | "CanGoPrevious" | "CanPlay" | "CanPause" | "CanSeek" => (state.status != PlaybackStatus::Stopped).to_variant(),
        "CanControl" => true.to_variant(),
        _ => false.to_variant(),
    }
}

fn metadata_variant(metadata: &TrackMetadata) -> glib::Variant {
    let dict = glib::VariantDict::new(None);
    dict.insert("mpris:trackid", glib::variant::ObjectPath::try_from(TRACK_ID_PATH).expect("a fixed, valid object path"));
    dict.insert("mpris:length", metadata.length_micros);
    dict.insert("xesam:title", metadata.title.as_str());
    if let Some(artist) = &metadata.artist {
        dict.insert("xesam:artist", vec![artist.clone()]);
    }
    if let Some(art_url) = &metadata.art_url {
        dict.insert("mpris:artUrl", art_url.as_str());
    }
    dict.end()
}

/// The D-Bus method-call closure's body: runs one inbound Player call against the current state.
/// It works on a copy, never holding `state` borrowed while the command runs: the player reports
/// a pause or a seek back at once, through `MprisHandle::update`, which writes `state` — still
/// inside this call. Holding the borrow made that a "RefCell already borrowed" panic, and since
/// this runs in a GLib callback that can't unwind, a crash.
fn handle_player_call(method: &str, params: &glib::Variant, commands: &dyn MprisCommands, state: &RefCell<PlayerState>) -> Result<Option<glib::Variant>, glib::Error> {
    let current = state.borrow().clone();
    dispatch_player_method(method, params, commands, &current)
}

/// Factored out of the D-Bus method-call closure so it's unit-testable against a fake
/// [`MprisCommands`] with no real D-Bus connection at all — the main fast-test surface for this
/// module (a real bus round-trip can only be smoke-tested, gated on whatever D-Bus tooling exists
/// in a given build/test environment).
fn dispatch_player_method(method: &str, params: &glib::Variant, commands: &dyn MprisCommands, state: &PlayerState) -> Result<Option<glib::Variant>, glib::Error> {
    match method {
        "PlayPause" => {
            commands.play_pause();
            Ok(None)
        }
        "Play" => {
            commands.play();
            Ok(None)
        }
        "Pause" | "Stop" => {
            // No separate "stopped" state exists in this app (see `PlaybackStatus`'s three
            // variants — `Stopped` is only ever reported, never entered from a running session);
            // MPRIS's `Stop` is treated the same as `Pause`.
            commands.pause();
            Ok(None)
        }
        "Next" => {
            commands.next();
            Ok(None)
        }
        "Previous" => {
            commands.previous();
            Ok(None)
        }
        "Seek" => {
            let offset: i64 = params
                .child_value(0)
                .get()
                .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::InvalidArgument, "Seek expects an int64 offset"))?;
            commands.seek(offset);
            Ok(None)
        }
        "SetPosition" => {
            let position: i64 = params
                .child_value(1)
                .get()
                .ok_or_else(|| glib::Error::new(gio::IOErrorEnum::InvalidArgument, "SetPosition expects an int64 position"))?;
            // Per the MPRIS spec, a position outside the track — or for a track that isn't the
            // current one, or with nothing loaded — is ignored rather than clamped: a client
            // that computed it from stale metadata must not jump the book somewhere else.
            let track_id = params.child_value(0).get::<glib::variant::ObjectPath>();
            let is_current_track = track_id.is_some_and(|id| id.as_str() == TRACK_ID_PATH);
            if state.status == PlaybackStatus::Stopped || !is_current_track || position < 0 || position > state.metadata.length_micros {
                tracing::info!(position, length = state.metadata.length_micros, is_current_track, "ignored an MPRIS SetPosition outside the current track");
                return Ok(None);
            }
            commands.set_position(position);
            Ok(None)
        }
        other => Err(glib::Error::new(gio::IOErrorEnum::NotSupported, &format!("unknown MPRIS method {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct FakeCommands {
        play_pause_calls: Cell<u32>,
        play_calls: Cell<u32>,
        pause_calls: Cell<u32>,
        seek_offset: Cell<Option<i64>>,
        set_position: Cell<Option<i64>>,
        next_calls: Cell<u32>,
        previous_calls: Cell<u32>,
    }

    impl MprisCommands for FakeCommands {
        fn play_pause(&self) {
            self.play_pause_calls.set(self.play_pause_calls.get() + 1);
        }
        fn play(&self) {
            self.play_calls.set(self.play_calls.get() + 1);
        }
        fn pause(&self) {
            self.pause_calls.set(self.pause_calls.get() + 1);
        }
        fn seek(&self, offset_micros: i64) {
            self.seek_offset.set(Some(offset_micros));
        }
        fn set_position(&self, position_micros: i64) {
            self.set_position.set(Some(position_micros));
        }
        fn next(&self) {
            self.next_calls.set(self.next_calls.get() + 1);
        }
        fn previous(&self) {
            self.previous_calls.set(self.previous_calls.get() + 1);
        }
    }

    /// A loaded book 100 s long, paused.
    fn loaded_state() -> PlayerState {
        PlayerState {
            status: PlaybackStatus::Paused,
            metadata: TrackMetadata { title: "A Book".to_string(), artist: None, length_micros: 100_000_000, art_url: None },
            position_micros: 0,
            rate: 1.0,
        }
    }

    #[test]
    fn play_pause_calls_through() {
        let commands = FakeCommands::default();
        dispatch_player_method("PlayPause", &().to_variant(), &commands, &loaded_state()).unwrap();
        assert_eq!(commands.play_pause_calls.get(), 1);
    }

    #[test]
    fn stop_is_treated_as_pause() {
        let commands = FakeCommands::default();
        dispatch_player_method("Stop", &().to_variant(), &commands, &loaded_state()).unwrap();
        assert_eq!(commands.pause_calls.get(), 1);
    }

    #[test]
    fn next_and_previous_call_through_without_changing_tracks() {
        let commands = FakeCommands::default();
        dispatch_player_method("Next", &().to_variant(), &commands, &loaded_state()).unwrap();
        dispatch_player_method("Previous", &().to_variant(), &commands, &loaded_state()).unwrap();
        assert_eq!(commands.next_calls.get(), 1);
        assert_eq!(commands.previous_calls.get(), 1);
    }

    #[test]
    fn seek_extracts_the_offset() {
        let commands = FakeCommands::default();
        let params = glib::Variant::tuple_from_iter([(-5_000_000_i64).to_variant()]);
        dispatch_player_method("Seek", &params, &commands, &loaded_state()).unwrap();
        assert_eq!(commands.seek_offset.get(), Some(-5_000_000));
    }

    #[test]
    fn set_position_extracts_the_position_not_the_track_id() {
        let commands = FakeCommands::default();
        let params = glib::Variant::tuple_from_iter([
            glib::variant::ObjectPath::try_from(TRACK_ID_PATH).unwrap().to_variant(),
            42_000_000_i64.to_variant(),
        ]);
        dispatch_player_method("SetPosition", &params, &commands, &loaded_state()).unwrap();
        assert_eq!(commands.set_position.get(), Some(42_000_000));
    }

    #[test]
    fn set_position_outside_the_track_or_for_another_track_is_ignored() {
        let commands = FakeCommands::default();
        let track = || glib::variant::ObjectPath::try_from(TRACK_ID_PATH).unwrap().to_variant();
        for position in [-1_i64, 100_000_001, i64::MAX] {
            let params = glib::Variant::tuple_from_iter([track(), position.to_variant()]);
            dispatch_player_method("SetPosition", &params, &commands, &loaded_state()).unwrap();
        }
        let other_track = glib::variant::ObjectPath::try_from("/org/mpris/MediaPlayer2/Track/9").unwrap().to_variant();
        dispatch_player_method("SetPosition", &glib::Variant::tuple_from_iter([other_track, 5_000_000_i64.to_variant()]), &commands, &loaded_state()).unwrap();
        let params = glib::Variant::tuple_from_iter([track(), 5_000_000_i64.to_variant()]);
        dispatch_player_method("SetPosition", &params, &commands, &PlayerState::default()).unwrap();
        assert_eq!(commands.set_position.get(), None, "none of those may move the book");

        dispatch_player_method("SetPosition", &glib::Variant::tuple_from_iter([track(), 100_000_000_i64.to_variant()]), &commands, &loaded_state()).unwrap();
        assert_eq!(commands.set_position.get(), Some(100_000_000), "the very end is still inside the track");
    }

    #[test]
    fn nothing_loaded_cannot_play_pause_or_seek() {
        let idle = PlayerState::default();
        for property in ["CanPlay", "CanPause", "CanSeek", "CanGoNext", "CanGoPrevious"] {
            assert_eq!(player_property(property, &idle).get::<bool>(), Some(false), "{property} with nothing loaded");
            assert_eq!(player_property(property, &loaded_state()).get::<bool>(), Some(true), "{property} with a book loaded");
        }
    }

    /// A Player property that differs between "nothing loaded" and "a book loaded" but isn't in
    /// `PropertiesChanged` stays stale in every client: that's how the controls stayed greyed out
    /// in the phone's media widget. Every property the interface declares is checked, so a new
    /// one is covered without touching this test.
    #[test]
    fn every_property_that_can_change_is_signalled() {
        let node_info = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML).expect("valid introspection XML");
        let player_info = node_info.lookup_interface(PLAYER_IFACE).expect("the Player interface");
        let declared: Vec<&str> = INTROSPECTION_XML
            .split("<property name=\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .filter(|name| player_info.lookup_property(name).is_some())
            .collect();
        assert!(declared.contains(&"CanPlay"), "the scan found the Player properties: {declared:?}");

        let (idle, loaded) = (PlayerState::default(), loaded_state());
        for property in declared {
            if property == "Position" || player_property(property, &idle) == player_property(property, &loaded) {
                continue;
            }
            assert!(SIGNALLED_PROPERTIES.contains(&property), "{property} changes when a book loads but isn't in PropertiesChanged");
        }

        let changed = glib::VariantDict::new(Some(&changed_properties(&loaded)));
        assert_eq!(changed.lookup_value("CanPlay", None).and_then(|value| value.get::<bool>()), Some(true));
        assert_eq!(changed.lookup_value("PlaybackStatus", None).and_then(|value| value.str().map(str::to_string)), Some("Paused".to_string()));
    }

    /// Commands that, like the real player, report the change straight back while they run:
    /// the app's snapshot listener calls `MprisHandle::update`, which writes the shared state.
    struct UpdatingCommands(Rc<RefCell<PlayerState>>);

    impl UpdatingCommands {
        fn report(&self) {
            self.0.borrow_mut().status = PlaybackStatus::Playing;
        }
    }

    impl MprisCommands for UpdatingCommands {
        fn play_pause(&self) {
            self.report();
        }
        fn play(&self) {
            self.report();
        }
        fn pause(&self) {
            self.report();
        }
        fn seek(&self, _offset_micros: i64) {
            self.report();
        }
        fn set_position(&self, _position_micros: i64) {
            self.report();
        }
        fn next(&self) {
            self.report();
        }
        fn previous(&self) {
            self.report();
        }
    }

    /// Pausing from the phone's media widget crashed 0.9.2: the call held the state borrowed
    /// while the pause ran, and the pause's own update then couldn't write it.
    #[test]
    fn a_command_may_update_the_state_while_it_runs() {
        let state = Rc::new(RefCell::new(loaded_state()));
        let commands = UpdatingCommands(state.clone());
        let track = || glib::variant::ObjectPath::try_from(TRACK_ID_PATH).unwrap().to_variant();
        let calls = [
            ("PlayPause", ().to_variant()),
            ("Play", ().to_variant()),
            ("Pause", ().to_variant()),
            ("Stop", ().to_variant()),
            ("Next", ().to_variant()),
            ("Previous", ().to_variant()),
            ("Seek", glib::Variant::tuple_from_iter([5_000_000_i64.to_variant()])),
            ("SetPosition", glib::Variant::tuple_from_iter([track(), 5_000_000_i64.to_variant()])),
        ];
        for (method, params) in calls {
            *state.borrow_mut() = loaded_state();
            handle_player_call(method, &params, &commands, &state).unwrap();
            assert_eq!(state.borrow().status, PlaybackStatus::Playing, "{method} reported its change");
        }
    }

    #[test]
    fn only_a_big_step_in_position_is_a_seek() {
        let at = |micros: i64| PlayerState { position_micros: micros, ..loaded_state() };
        assert!(!position_jumped(&at(10_000_000), &at(10_750_000)), "3x playback for a tick");
        assert!(!position_jumped(&at(10_000_000), &at(10_000_000)));
        assert!(position_jumped(&at(10_000_000), &at(40_000_000)), "a skip forward");
        assert!(position_jumped(&at(40_000_000), &at(10_000_000)), "a rewind");
    }

    #[test]
    fn unknown_method_is_an_error_not_a_panic() {
        let commands = FakeCommands::default();
        assert!(dispatch_player_method("SomethingUnsupported", &().to_variant(), &commands, &loaded_state()).is_err());
    }

    #[test]
    fn introspection_xml_parses_and_has_both_interfaces() {
        let node_info = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML).expect("valid introspection XML");
        assert!(node_info.lookup_interface(MEDIA_PLAYER2_IFACE).is_some());
        assert!(node_info.lookup_interface(PLAYER_IFACE).is_some());
    }

    #[test]
    fn player_property_reports_current_state() {
        let state = PlayerState {
            status: PlaybackStatus::Playing,
            metadata: TrackMetadata { title: "A Book".to_string(), artist: Some("An Author".to_string()), length_micros: 1_000_000, art_url: None },
            position_micros: 500_000,
            rate: 1.5,
        };
        assert_eq!(player_property("PlaybackStatus", &state).str(), Some("Playing"));
        assert_eq!(player_property("Position", &state).get::<i64>(), Some(500_000));
        assert_eq!(player_property("Rate", &state).get::<f64>(), Some(1.5));
    }

    /// Registers against a *real* session bus — only checkable when one is reachable, which this
    /// sandbox doesn't provide by default (`register`'s own graceful `Err` on a missing bus is
    /// what fast tests above exercise indirectly by never needing a bus at all). Run explicitly
    /// via `dbus-run-session -- cargo test -p abs-player -- --ignored --test-threads=1 mpris::tests`
    /// once `dbus-run-session` is confirmed present (checked during implementation: it is, in
    /// this build environment, though real hardware verification of the lock-screen card itself
    /// still requires GNOME Shell/phosh). One thread: the live tests all export the same object
    /// path on the process's one session-bus connection.
    #[test]
    #[ignore]
    fn register_succeeds_against_a_real_session_bus() {
        let commands = Rc::new(FakeCommands::default());
        let handle = register("AbsPlayerLiveTest", "AbsPlayerLiveTest", commands).expect("register should succeed with a real session bus reachable");
        handle.update(PlayerState {
            status: PlaybackStatus::Playing,
            metadata: TrackMetadata { title: "Live Test".to_string(), artist: None, length_micros: 1, art_url: None },
            position_micros: 0,
            rate: 1.0,
        });
    }

    /// A shell rebuild (switching servers) registers the new player while the old handle is
    /// still alive: that must fail cleanly, leave nothing half-registered behind, and succeed
    /// again once the old handle is dropped. Needs a real session bus — run like the test above, with
    /// `--test-threads=1` (the tests share the process's one bus connection and object path).
    #[test]
    #[ignore]
    fn a_second_registration_waits_for_the_first_handle_to_drop() {
        let first = register("AbsPlayerHandoffTest", "AbsPlayerHandoffTest", Rc::new(FakeCommands::default())).expect("a session bus is reachable");
        let second = register("AbsPlayerHandoffTest", "AbsPlayerHandoffTest", Rc::new(FakeCommands::default()));
        assert!(matches!(second, Err(MprisError::RegistrationFailed(_))), "the object path is taken while the first handle lives");
        drop(first);
        let third = register("AbsPlayerHandoffTest", "AbsPlayerHandoffTest", Rc::new(FakeCommands::default()));
        assert!(third.is_ok(), "the path is free again after the first handle drops: {:?}", third.err());
    }

    /// Does what a phone's media widget (Phosh, GNOME Shell) does: a `GDBusProxy` on the Player
    /// interface, which keeps its own copy of every property and updates it only from
    /// `PropertiesChanged`. The app registers with nothing loaded, so the widget first learns the
    /// controls are off; when a book then plays, that copy must hear they're on again, or the
    /// widget stays greyed out with every button a no-op. Needs a real session bus — run like the
    /// test above.
    #[test]
    #[ignore]
    fn a_client_sees_the_controls_enable_when_a_book_starts() {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let spin_until = |what: &str, done: &dyn Fn() -> bool| {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while !done() {
                        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
                        if !context.iteration(false) {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                };

                let handle = register("AbsPlayerProxyTest", "AbsPlayerProxyTest", Rc::new(FakeCommands::default())).expect("a session bus is reachable");

                // Its own connection, like a separate process: the proxy's calls must reach the
                // object through the bus, not short-circuit on the registering connection.
                let address = gio::dbus_address_get_for_bus_sync(gio::BusType::Session, gio::Cancellable::NONE).expect("a session bus address");
                let client = gio::DBusConnection::for_address_sync(
                    &address,
                    gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                    None,
                    gio::Cancellable::NONE,
                )
                .expect("a second connection to the session bus");
                let proxy = Rc::new(RefCell::new(None));
                gio::DBusProxy::new(&client, gio::DBusProxyFlags::NONE, None, Some("org.mpris.MediaPlayer2.AbsPlayerProxyTest"), OBJECT_PATH, PLAYER_IFACE, gio::Cancellable::NONE, {
                    let proxy = proxy.clone();
                    move |result| *proxy.borrow_mut() = Some(result.expect("a proxy for the Player interface"))
                });
                let can_play = || proxy.borrow().as_ref().and_then(|proxy| proxy.cached_property("CanPlay")).and_then(|value| value.get::<bool>());

                spin_until("the client to load the properties", &|| can_play().is_some());
                assert_eq!(can_play(), Some(false), "nothing is loaded yet");

                handle.update(PlayerState { status: PlaybackStatus::Playing, ..loaded_state() });
                spin_until("the client to see CanPlay turn on", &|| can_play() == Some(true));
            })
            .expect("the test's main context is free");
    }

    /// What crashed 0.9.2 on the phone: the media widget calls `PlayPause`, the player pauses and
    /// reports it straight back through `update`, all inside the D-Bus call. Needs a real session
    /// bus — run like the tests above.
    #[test]
    #[ignore]
    fn a_client_can_pause_a_player_that_reports_back_at_once() {
        struct ReportingCommands(Rc<RefCell<Option<MprisHandle>>>);
        impl MprisCommands for ReportingCommands {
            fn play_pause(&self) {
                if let Some(handle) = self.0.borrow().as_ref() {
                    handle.update(loaded_state());
                }
            }
            fn play(&self) {}
            fn pause(&self) {}
            fn seek(&self, _offset_micros: i64) {}
            fn set_position(&self, _position_micros: i64) {}
            fn next(&self) {}
            fn previous(&self) {}
        }

        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let slot = Rc::new(RefCell::new(None));
                let handle = register("AbsPlayerPauseTest", "AbsPlayerPauseTest", Rc::new(ReportingCommands(slot.clone()))).expect("a session bus is reachable");
                handle.update(PlayerState { status: PlaybackStatus::Playing, ..loaded_state() });
                *slot.borrow_mut() = Some(handle);

                let address = gio::dbus_address_get_for_bus_sync(gio::BusType::Session, gio::Cancellable::NONE).expect("a session bus address");
                let client = gio::DBusConnection::for_address_sync(
                    &address,
                    gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                    None,
                    gio::Cancellable::NONE,
                )
                .expect("a second connection to the session bus");
                let reply = Rc::new(RefCell::new(None));
                client.call(
                    Some("org.mpris.MediaPlayer2.AbsPlayerPauseTest"),
                    OBJECT_PATH,
                    PLAYER_IFACE,
                    "PlayPause",
                    None,
                    None,
                    gio::DBusCallFlags::NONE,
                    5_000,
                    gio::Cancellable::NONE,
                    {
                        let reply = reply.clone();
                        move |result| *reply.borrow_mut() = Some(result.map(|_| ()))
                    },
                );

                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
                while reply.borrow().is_none() {
                    assert!(std::time::Instant::now() < deadline, "timed out waiting for the PlayPause reply");
                    if !context.iteration(false) {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
                reply.borrow_mut().take().unwrap().expect("PlayPause succeeds");
                // Taken out of the slot, the handle is dropped here, which unregisters it — inside
                // the slot it's held by its own commands and would outlive the test.
                let handle = slot.borrow_mut().take().expect("the handle is still in the slot");
                assert_eq!(handle.state.borrow().status, PlaybackStatus::Paused, "the pause was reported");
            })
            .expect("the test's main context is free");
    }

    #[test]
    fn metadata_variant_includes_title_and_artist() {
        let metadata = TrackMetadata { title: "A Book".to_string(), artist: Some("An Author".to_string()), length_micros: 42, art_url: None };
        let variant = metadata_variant(&metadata);
        let dict = glib::VariantDict::new(Some(&variant));
        assert_eq!(dict.lookup_value("xesam:title", None).and_then(|v| v.str().map(str::to_string)), Some("A Book".to_string()));
        assert_eq!(dict.lookup_value("mpris:length", None).and_then(|v| v.get::<i64>()), Some(42));
    }

    #[test]
    fn player_state_changed_is_false_when_only_position_differs() {
        let previous = PlayerState { status: PlaybackStatus::Playing, position_micros: 0, ..PlayerState::default() };
        let new = PlayerState { status: PlaybackStatus::Playing, position_micros: 250_000, ..PlayerState::default() };
        assert!(!player_state_changed(&previous, &new));
    }

    #[test]
    fn player_state_changed_is_true_when_status_or_metadata_or_rate_differs() {
        let base = PlayerState::default();

        let status_changed = PlayerState { status: PlaybackStatus::Playing, ..base.clone() };
        assert!(player_state_changed(&base, &status_changed));

        let metadata_changed =
            PlayerState { metadata: TrackMetadata { title: "New Title".to_string(), ..TrackMetadata::default() }, ..base.clone() };
        assert!(player_state_changed(&base, &metadata_changed));

        let rate_changed = PlayerState { rate: 1.5, ..base.clone() };
        assert!(player_state_changed(&base, &rate_changed));
    }
}
