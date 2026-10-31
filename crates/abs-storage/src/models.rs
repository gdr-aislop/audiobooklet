//! Plain data types shared across `repo/*` modules. These are storage-layer models (they mirror
//! table rows), not the API's wire types (`abs-api`'s generated `types::*`) or UI view-models —
//! `abs-core` is responsible for mapping between the three.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct Server {
    pub id: String,
    pub url: String,
    pub custom_headers_json: String,
    pub disable_ssl_verify: bool,
    pub client_cert_path: Option<String>,
    /// The PKCS#12 bundle's export password (see `repo::servers`). Plaintext, like the account
    /// tokens this database already holds.
    pub client_cert_password: Option<String>,
    pub local_network_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct Account {
    pub id: String,
    pub server_id: String,
    pub username: String,
    pub token: String,
    /// The JWT-auth refresh token (server v2.26.0+): long-lived, rotated on every refresh, and
    /// what keeps a session alive past the access token's expiry. `None` for legacy servers
    /// (permanent tokens that never expire).
    pub refresh_token: Option<String>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
}

// `Debug` is hand-written so a stray `{:?}` in a log call can never print credentials.
impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("custom_headers_json", &"<redacted>")
            .field("disable_ssl_verify", &self.disable_ssl_verify)
            .field("client_cert_path", &self.client_cert_path)
            .field("client_cert_password", &self.client_cert_password.as_ref().map(|_| "<redacted>"))
            .field("local_network_address", &self.local_network_address)
            .field("user_agent", &self.user_agent)
            .field("created_at", &self.created_at)
            .finish()
    }
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("id", &self.id)
            .field("server_id", &self.server_id)
            .field("username", &self.username)
            .field("token", &"<redacted>")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<redacted>"))
            .field("is_active", &self.is_active)
            .field("created_at", &self.created_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct Library {
    pub id: String,
    pub server_id: String,
    pub name: String,
    pub media_type: String,
    pub icon: Option<String>,
    pub display_order: i64,
    pub synced_at: DateTime<Utc>,
    /// The server no longer lists this library; it's kept only for downloaded books in it.
    pub removed_on_server: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct Item {
    pub id: String,
    pub server_id: String,
    pub library_id: String,
    pub title: String,
    pub author: Option<String>,
    pub narrator: Option<String>,
    pub description: Option<String>,
    pub cover_cache_path: Option<String>,
    pub duration_seconds: f64,
    pub added_at: DateTime<Utc>,
    pub synced_at: DateTime<Utc>,
    pub series_name: Option<String>,
    /// The `genres` column, JSON-encoded (SQLite has no array type) — same "encode a compound
    /// value into one TEXT column" convention `servers.custom_headers_json` already uses. Decode
    /// via [`Item::genres`] rather than parsing this directly.
    pub genres_json: String,
    /// The server's own sort keys (`titleIgnorePrefix`: "Hobbit, The"; `authorNameLF`: "Tolkien,
    /// J.R.R."), `None` until a sync after migration 0013 or when the server doesn't send them.
    /// Lists sort by these when present.
    pub title_sort: Option<String>,
    pub author_sort: Option<String>,
}

/// What the library list returns beyond [`Item`]'s own fields, kept for Book Details and the
/// download estimates (migration 0013). Everything is optional: `None` is "the server didn't say".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ItemExtras {
    pub subtitle: Option<String>,
    /// The item's total size on the server, all files included (audio and anything else).
    pub size_bytes: Option<i64>,
    pub num_chapters: Option<i64>,
    /// JSON-encoded like [`Item::genres_json`]; decode via [`ItemExtras::tags`].
    pub tags_json: String,
    pub published_year: Option<String>,
    pub publisher: Option<String>,
    pub language: Option<String>,
    pub explicit: Option<bool>,
    pub isbn: Option<String>,
    pub asin: Option<String>,
    /// Whether the server has a cover for the item; `None` is unknown, treated as "maybe".
    pub has_cover: Option<bool>,
    pub title_sort: Option<String>,
    pub author_sort: Option<String>,
}

impl ItemExtras {
    pub fn tags(&self) -> Vec<String> {
        serde_json::from_str(&self.tags_json).unwrap_or_default()
    }
}

impl Item {
    /// Decodes [`Item::genres_json`]. Malformed JSON (which should never happen — this app is the
    /// only writer) degrades to an empty list rather than failing the whole render.
    pub fn genres(&self) -> Vec<String> {
        serde_json::from_str(&self.genres_json).unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct Chapter {
    pub server_id: String,
    pub item_id: String,
    pub chapter_index: i64,
    pub title: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct Progress {
    pub account_id: String,
    pub server_id: String,
    pub item_id: String,
    pub current_time_seconds: f64,
    pub is_finished: bool,
    pub updated_at: DateTime<Utc>,
    /// A local write the server hasn't confirmed yet — see migration 0009.
    pub needs_push: bool,
}

/// A stretch of listening to one book recorded on this device — see migration 0012 and
/// `repo::listening_sessions`. Timestamps are this device's clock, in milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct ListeningSession {
    pub id: String,
    pub account_id: String,
    pub server_id: String,
    pub item_id: String,
    /// Audiobookshelf's `PlayMethod`: 0 streamed, 3 played from a downloaded file.
    pub play_method: i64,
    /// Book position the session started at, seconds.
    pub start_position: f64,
    /// Book position now, seconds.
    pub current_position: f64,
    pub time_listening: f64,
    pub duration: f64,
    pub started_at: i64,
    pub updated_at: i64,
    /// The `updated_at` the server last confirmed; the row needs sending while it differs.
    pub sent_updated_at: Option<i64>,
    pub closed: bool,
}

/// Cached per-item track metadata — mirrors `abs_core::streaming::StreamTrack`, persisted so
/// downloads (and, later, offline-availability checks) don't need a network call to know what a
/// book's tracks are. `size_bytes` is the server-reported file size, when known — what size
/// estimates are computed from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct Track {
    pub server_id: String,
    pub item_id: String,
    pub ino: String,
    pub track_index: i64,
    pub duration_seconds: f64,
    pub offset_seconds: f64,
    pub size_bytes: Option<i64>,
}

/// A track download's lifecycle. `Downloading` and `Failed` both retain whatever
/// `bytes_downloaded` was reached, so a `Downloading` row left behind by an ungraceful app exit is
/// just as resumable as a `Failed` one — the status only reflects "what happened last", not
/// whether resuming is possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStatus {
    Pending,
    Downloading,
    Complete,
    Failed,
}

impl DownloadStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DownloadStatus::Pending => "pending",
            DownloadStatus::Downloading => "downloading",
            DownloadStatus::Complete => "complete",
            DownloadStatus::Failed => "failed",
        }
    }
}

impl std::str::FromStr for DownloadStatus {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(DownloadStatus::Pending),
            "downloading" => Ok(DownloadStatus::Downloading),
            "complete" => Ok(DownloadStatus::Complete),
            "failed" => Ok(DownloadStatus::Failed),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadTrack {
    pub server_id: String,
    pub item_id: String,
    pub ino: String,
    pub file_path: String,
    pub expected_size_bytes: Option<i64>,
    pub bytes_downloaded: i64,
    pub status: DownloadStatus,
    pub error_reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_credentials() {
        let server = Server {
            id: "s1".into(),
            url: "https://abs.example.com".into(),
            custom_headers_json: r#"{"X-Proxy-Auth":"proxy-secret"}"#.into(),
            disable_ssl_verify: false,
            client_cert_path: None,
            client_cert_password: Some("cert-secret".into()),
            local_network_address: None,
            user_agent: None,
            created_at: Utc::now(),
        };
        let account = Account {
            id: "a1".into(),
            server_id: "s1".into(),
            username: "alice".into(),
            token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            is_active: true,
            created_at: Utc::now(),
        };
        let out = format!("{server:?} {account:?}");
        for secret in ["proxy-secret", "cert-secret", "access-secret", "refresh-secret"] {
            assert!(!out.contains(secret), "{secret} leaked via Debug: {out}");
        }
        assert!(out.contains("s1") && out.contains("a1"), "ids stay visible for debugging");
    }
}
