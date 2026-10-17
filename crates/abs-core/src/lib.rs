//! UI-agnostic domain logic: authentication, multi-server/account management, library sync,
//! download-scope resolution, the playback state machine, and typed settings. Depends on
//! `abs-api` and `abs-storage`; never depends on GTK or GStreamer, which is what makes it
//! unit-testable without a display or audio hardware — see each module's tests.

pub mod accounts;
pub mod auth;
pub mod chapters;
pub mod connection;
pub mod covers;
pub mod download_tracks;
pub mod downloads;
pub mod error;
pub mod listening_sessions;
pub mod media_type;
pub mod passwords;
pub mod playback;
pub mod progress_sync;
pub mod search;
pub mod series;
pub mod settings;
pub mod streaming;
pub mod sync;
pub mod tracks;

pub use error::{CoreError, Result};
