#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    #[error("gstreamer initialization failed: {0}")]
    InitFailed(#[from] gstreamer::glib::Error),

    #[error("failed to build the playback pipeline: {0}")]
    PipelineBuild(#[from] gstreamer::glib::BoolError),

    #[error("failed to change pipeline state: {0}")]
    StateChange(#[from] gstreamer::StateChangeError),

    #[error("seek failed")]
    SeekFailed,

    #[error("no source loaded")]
    NoSourceLoaded,
}

impl PlayerError {
    /// A coarse, user-facing classification of a synchronous backend failure — same buckets
    /// [`PlaybackErrorKind`] uses for the bus-delivered [`crate::PlayerEvent::Error`], so the
    /// `app` crate can build one [`PlaybackError`] regardless of which of the two failure paths
    /// (synchronous `Result`, or an async bus message) produced it.
    pub fn kind(&self) -> PlaybackErrorKind {
        match self {
            // These three mean the engine itself couldn't come up at all (no `playbin` element,
            // GStreamer core init failed, or a state change — usually the audio sink — was
            // refused) — indistinguishable from each other at the call site, and all mean the
            // same thing to a user: nothing is going to play right now.
            PlayerError::InitFailed(_) | PlayerError::PipelineBuild(_) | PlayerError::NoSourceLoaded => PlaybackErrorKind::Unavailable,
            PlayerError::StateChange(_) => PlaybackErrorKind::AudioOutput,
            PlayerError::SeekFailed => PlaybackErrorKind::Other,
        }
    }
}

pub type Result<T> = std::result::Result<T, PlayerError>;

/// A coarse, matchable classification of a playback failure — deliberately small: this is what a
/// UI picks a friendly message and an optional retry affordance from, not an exhaustive mirror of
/// GStreamer's own error taxonomy. See [`crate::classify_gst_error`] for how a real GStreamer bus
/// error maps into one of these, and [`PlayerError::kind`] for the synchronous-`Result` side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackErrorKind {
    /// No decoder/demuxer plugin is installed for this file's audio format.
    MissingCodec,
    /// A plugin exists but the specific file couldn't be decoded — corrupt or genuinely
    /// unsupported content, not a missing-plugin problem.
    UnsupportedOrCorrupt,
    /// The stream URL 404'd or the local file is gone — the item may have been moved/deleted.
    ResourceNotFound,
    /// The server refused the request (expired/invalid token past what a silent refresh caught).
    NotAuthorized,
    /// The pipeline couldn't open/write to an audio sink (no PulseAudio/PipeWire, device busy).
    AudioOutput,
    /// A network-shaped resource failure while streaming (dropped connection, read failure).
    Network,
    /// The server never answered when a book was being started — nothing has played, so
    /// "lost the connection while playing" would be wrong. Raised by the app, not by GStreamer.
    Unreachable,
    /// The pipeline never got to a requested position — it kept playing from somewhere else
    /// even after the file was loaded again. Raised by the app, not by GStreamer.
    Seek,
    /// The app's offline mode is on and this part of the book isn't downloaded, so it can't be
    /// streamed. Raised by the app, not by GStreamer.
    Offline,
    /// The item has no audio this app can play (a podcast, or an e-book-only book). Raised by
    /// the app, not by GStreamer; retrying can't help.
    NoAudio,
    /// No working audio engine at all — GStreamer itself failed to initialize or build a
    /// pipeline. Distinct from every case above: those mean *this file* won't play; this means
    /// *nothing* will, until the device itself is fixed.
    Unavailable,
    /// Anything else — still shown honestly (via the raw message), just not specially worded.
    Other,
}

/// A playback failure, honest and classified: [`PlaybackErrorKind`] for the UI to pick a message
/// and affordance from, plus the underlying text so "don't hide what happened" always holds —
/// shown behind a "Show details" disclosure, never as the headline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackError {
    pub kind: PlaybackErrorKind,
    /// The `Display` text of the underlying error (a `glib::Error`'s message, or a `PlayerError`
    /// converted via [`PlayerError::kind`]).
    pub message: String,
    /// GStreamer's separate, more verbose debug string when the failure came from a bus message
    /// (often carries the real URI/HTTP status/errno) — `None` for a synchronous `PlayerError`,
    /// which has no equivalent.
    pub debug: Option<String>,
}

impl From<&PlayerError> for PlaybackError {
    fn from(err: &PlayerError) -> Self {
        PlaybackError { kind: err.kind(), message: err.to_string(), debug: None }
    }
}
