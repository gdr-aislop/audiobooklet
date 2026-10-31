//! Hand-written extensions to the generated [`Client`], for endpoints confirmed missing from the
//! vendored OpenAPI spec entirely — see `third_party/audiobookshelf-openapi/README.md`'s "Known
//! gaps". These live here, as a plain `impl Client` using the same [`Client::client`] /
//! [`Client::baseurl`] accessors the generated code itself is built on, so every caller depends
//! on exactly one client type regardless of whether a given method happens to be generated or
//! hand-written. If upstream ever documents one of these endpoints, the generated method takes
//! over and the hand-written one here is deleted.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::Client;

#[derive(Debug, Serialize)]
struct LoginRequest<'a> {
    username: &'a str,
    password: &'a str,
}

/// How long a token refresh waits for the server to start answering. Longer than the clients'
/// 15 s connect limit, so a connection that never comes up fails as a connect error first (which
/// [`LoginError::server_may_have_acted`] can rule out) rather than as [`LoginError::NoAnswer`].
const REFRESH_ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How long a token refresh waits for the rest of a reply once the server has answered with
/// success — see [`Client::refresh`].
const REFRESH_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Deserialize)]
struct LoginResponseBody {
    user: LoginUser,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginUser {
    id: String,
    username: String,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoginResult {
    pub user_id: String,
    pub username: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// The user an API key belongs to; see [`Client::whoami`].
#[derive(Debug, Clone, PartialEq)]
pub struct Whoami {
    pub user_id: String,
    pub username: String,
}

#[derive(Debug, Deserialize)]
struct WhoamiBody {
    id: String,
    username: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("invalid username or password")]
    InvalidCredentials,
    #[error("this session's login has expired — sign in again")]
    SessionExpired,
    #[error("couldn't verify this server's TLS certificate: {0}")]
    Tls(reqwest::Error),
    /// The server wants a client certificate or doesn't accept the one given: a TLS alert from the
    /// peer, or (behind nginx) an HTTP 400 saying so. Told apart from [`LoginError::Tls`], which
    /// is about the *server's* certificate.
    #[error("the server needs a client certificate, or doesn't accept the one given")]
    ClientCertificate(Option<reqwest::Error>),
    /// HTTP 403 or 407: not an answer Audiobookshelf itself gives to a login (it says 401), so a
    /// proxy or firewall in front of it is refusing the request.
    #[error("the server refused the request (HTTP {0})")]
    Blocked(u16),
    /// Any other unsuccessful HTTP status (404 for a wrong address or path, 5xx for a server
    /// problem, …).
    #[error("the server answered HTTP {0}")]
    HttpStatus(u16),
    #[error("couldn't connect to this server: {0}")]
    Connect(reqwest::Error),
    #[error("timed out waiting for this server to respond: {0}")]
    Timeout(reqwest::Error),
    /// The server hadn't started answering when the wait for it ran out (see [`Client::refresh`],
    /// which waits on the answer and on the rest of the reply separately).
    #[error("timed out waiting for this server to respond")]
    NoAnswer,
    #[error("network error: {0}")]
    Network(reqwest::Error),
    #[error("server returned an unexpected response: {0}")]
    UnexpectedResponse(String),
}

impl LoginError {
    /// The full underlying error text (this error plus its `source()` chain), for an optional
    /// "Show details" disclosure in the UI — never the primary message, since raw HTTP/TLS
    /// library text isn't meant for a general audience, but worth having for a self-hosted user
    /// debugging an unusual TLS/proxy setup.
    pub fn details(&self) -> Option<String> {
        let err: &(dyn std::error::Error + 'static) = match self {
            LoginError::Tls(e) | LoginError::Connect(e) | LoginError::Timeout(e) | LoginError::Network(e) => e,
            LoginError::ClientCertificate(Some(e)) => e,
            LoginError::ClientCertificate(None)
            | LoginError::Blocked(_)
            | LoginError::HttpStatus(_)
            | LoginError::InvalidCredentials
            | LoginError::SessionExpired
            | LoginError::NoAnswer
            | LoginError::UnexpectedResponse(_) => return None,
        };
        Some(error_chain(err))
    }

    /// Whether the server may have acted on the request even though no usable answer came back:
    /// the connection was made, so the request may have arrived, and the failure came after. For
    /// a token refresh this matters: the server swaps the token pair as soon as it handles the
    /// request, so a reply that never arrives can leave this client holding a refresh token the
    /// server has already replaced. `false` for failures before anything was sent (couldn't
    /// connect, TLS) and for definite answers (a rejection, an error status).
    pub fn server_may_have_acted(&self) -> bool {
        match self {
            LoginError::NoAnswer => true,
            LoginError::Timeout(err) | LoginError::Network(err) => !err.is_connect(),
            LoginError::InvalidCredentials
            | LoginError::SessionExpired
            | LoginError::Tls(_)
            | LoginError::ClientCertificate(_)
            | LoginError::Blocked(_)
            | LoginError::HttpStatus(_)
            | LoginError::Connect(_)
            | LoginError::UnexpectedResponse(_) => false,
        }
    }
}

/// Turns a raw transport error into one of `LoginError`'s categorized variants.
fn classify_transport_error(err: reqwest::Error) -> LoginError {
    if err.is_timeout() {
        return LoginError::Timeout(err);
    }
    if is_client_certificate_error(&err) {
        return LoginError::ClientCertificate(Some(err));
    }
    if is_tls_error(&err) {
        return LoginError::Tls(err);
    }
    if err.is_connect() {
        return LoginError::Connect(err);
    }
    LoginError::Network(err)
}

/// Walks the error's `source()` chain looking for a TLS/certificate cause. String-matching on
/// `Display` text (rather than downcasting to a concrete native-tls/rustls/openssl error type) is
/// deliberate: reqwest is built here with its default TLS backend (native-tls/OpenSSL on Linux —
/// confirmed via Cargo.toml/Cargo.lock, no `rustls-tls` feature enabled), and matching on wording
/// keeps this working across whatever backend or version actually surfaces the error, at the cost
/// of being best-effort — a false negative just falls through to `Connect`/`Network`, never wrong
/// in the dangerous direction (it never *hides* a real TLS error as a credentials problem).
/// TLS alerts a *server* sends when it wants a client certificate or won't take ours (OpenSSL
/// words them `tlsv13 alert certificate required`, `tlsv1 alert unknown ca`, …). Matched on the
/// "alert" wording on purpose: a failure to verify the server's own certificate says
/// "certificate verify failed … certificate has expired", which must stay a plain TLS error.
const CLIENT_CERTIFICATE_ALERTS: [&str; 8] = [
    "alert certificate required",
    "alert unknown ca",
    "alert bad certificate",
    "alert certificate unknown",
    "alert certificate expired",
    "alert certificate revoked",
    "alert unsupported certificate",
    "alert access denied",
];

/// Whether the error chain's text (see [`error_chain`]) is a peer's rejection of the client
/// certificate. Pure on the text, so it is testable without a socket.
fn is_client_certificate_text(text: &str) -> bool {
    let text = text.to_lowercase();
    CLIENT_CERTIFICATE_ALERTS.iter().any(|alert| text.contains(alert))
}

fn is_client_certificate_error(err: &(dyn std::error::Error + 'static)) -> bool {
    is_client_certificate_text(&error_chain(err))
}

/// Whether an HTTP error body is a proxy complaining about a missing client certificate (nginx:
/// "400 Bad Request — No required SSL certificate was sent").
fn is_client_certificate_body(body: &str) -> bool {
    let body = body.to_lowercase();
    body.contains("ssl certificate") || body.contains("client certificate")
}

/// The most specific [`LoginError`] for an unsuccessful (and not 401) HTTP answer to a login,
/// refresh or `/api/me` call. Reads the start of the body for a 400, the status nginx uses for a
/// missing client certificate.
async fn status_error(response: reqwest::Response) -> LoginError {
    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        return LoginError::Blocked(status.as_u16());
    }
    if status == reqwest::StatusCode::BAD_REQUEST {
        let mut response = response;
        if let Ok(Some(chunk)) = response.chunk().await {
            let start = &chunk[..chunk.len().min(512)];
            if is_client_certificate_body(&String::from_utf8_lossy(start)) {
                return LoginError::ClientCertificate(None);
            }
        }
    }
    LoginError::HttpStatus(status.as_u16())
}

fn is_tls_error(err: &(dyn std::error::Error + 'static)) -> bool {
    const MARKERS: [&str; 6] =
        ["certificate", "self signed", "self-signed", "ssl", "tls", "handshake"];
    let text = err.to_string().to_lowercase();
    if MARKERS.iter().any(|m| text.contains(m)) {
        return true;
    }
    err.source().is_some_and(is_tls_error)
}

/// Walks an error's `source()` chain into a single line: the error itself, then each cause,
/// joined with " → ". reqwest's `Display` is just "error sending request for url (...)" for
/// every transport failure — a timeout, a DNS failure, a refused connection and a TLS
/// handshake failure all print identically — while what actually happened only appears in
/// the `source()` chain, which `Display` never prints. Every log line and "details" surface
/// for transport errors is built on this ([`LoginError::details`],
/// [`LibraryItemsError::details`]).
pub fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut current = err.source();
    while let Some(source) = current {
        parts.push(source.to_string());
        current = source.source();
    }
    parts.join(" → ")
}

/// A short, greppable classification of a transport failure — `"timeout"`, `"tls"`,
/// `"connect"` (DNS or TCP; hyper reports DNS failures as connect errors), `"body"`,
/// `"decode"`, `"request"` (any other send-phase failure) or `"unknown"`. Checks are ordered
/// most-actionable-first: a timed-out connect reports as `"timeout"`, and a TLS failure
/// while connecting as `"tls"` (matched via the source chain, reqwest having no public
/// `is_tls`).
pub fn transport_error_kind(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if is_tls_error(err) {
        "tls"
    } else if err.is_connect() {
        "connect"
    } else if err.is_body() {
        "body"
    } else if err.is_decode() {
        "decode"
    } else if err.is_request() {
        "request"
    } else {
        "unknown"
    }
}

/// A field that is kept when it has the expected type and ignored (`None`) when it hasn't, so one
/// odd value in one book's record can't fail the whole library list, which would fail the sync.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Like [`lenient`] for text the server may send as a number (`publishedYear`).
fn lenient_text<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

/// A cover path told apart three ways: the key missing or of an odd type (`None`: unknown), `null`
/// (`Some(None)`: the server says there is no cover) and a path (`Some(Some(path))`).
fn lenient_cover<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Some(None),
        serde_json::Value::String(path) => Some(Some(path)),
        _ => None,
    })
}

#[derive(Debug, Deserialize)]
struct LibraryItemsResponseBody {
    results: Vec<RawLibraryItem>,
}

#[derive(Debug, Deserialize)]
struct RawLibraryItem {
    id: Option<String>,
    #[serde(rename = "addedAt")]
    added_at: Option<i64>,
    /// `"book"` or `"podcast"`.
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    media: Option<RawMedia>,
}

#[derive(Debug, Deserialize)]
struct RawMedia {
    duration: Option<f64>,
    metadata: Option<RawMetadata>,
    /// A book's audio file and track counts; both 0 for an e-book-only item. Podcasts have
    /// neither (their audio is in episodes).
    #[serde(rename = "numAudioFiles")]
    num_audio_files: Option<i64>,
    #[serde(rename = "numTracks")]
    num_tracks: Option<i64>,
    /// The item's total size on the server, every file included.
    #[serde(default, deserialize_with = "lenient")]
    size: Option<u64>,
    #[serde(rename = "numChapters", default, deserialize_with = "lenient")]
    num_chapters: Option<i64>,
    #[serde(default, deserialize_with = "lenient")]
    tags: Option<Vec<String>>,
    #[serde(rename = "coverPath", default, deserialize_with = "lenient_cover")]
    cover_path: Option<Option<String>>,
}

impl RawMedia {
    /// Whether a book has audio to play. Missing counts (an older server, a trimmed response)
    /// count as audio: hiding a playable book would be worse than listing one that isn't.
    fn has_audio(&self) -> bool {
        match (self.num_audio_files, self.num_tracks) {
            (Some(files), _) => files > 0,
            (None, Some(tracks)) => tracks > 0,
            (None, None) => true,
        }
    }
}

/// Audiobookshelf sends `""` rather than leaving the field out for a book with no author, narrator
/// or series; here that's `None`, so it lands in "Unknown author"/"Other" like a missing one.
fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Deserialize)]
struct RawMetadata {
    title: Option<String>,
    #[serde(rename = "authorName")]
    author_name: Option<String>,
    #[serde(rename = "narratorName")]
    narrator_name: Option<String>,
    description: Option<String>,
    #[serde(rename = "seriesName")]
    series_name: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default, deserialize_with = "lenient")]
    subtitle: Option<String>,
    #[serde(rename = "publishedYear", default, deserialize_with = "lenient_text")]
    published_year: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    publisher: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    language: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    explicit: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    isbn: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    asin: Option<String>,
    /// The server's sort key for the title, leading "The"/"A" moved to the end.
    #[serde(rename = "titleIgnorePrefix", default, deserialize_with = "lenient")]
    title_sort: Option<String>,
    /// The author as "Last, First", the server's sort key.
    #[serde(rename = "authorNameLF", default, deserialize_with = "lenient")]
    author_sort: Option<String>,
}

/// Connection-level options applied to every request a minted [`Client`] sends — the transport
/// side of the `servers` table's per-server settings (custom headers, TLS verification, client
/// certificate, user agent). Deliberately plain std types: this crate is the only one that
/// converts them into reqwest primitives (`HeaderMap`, `Identity`, TLS flags), so no other crate
/// ever names a reqwest type to describe connection settings.
///
/// `ConnectionOptions::default()` is what [`Client::with_bearer_token`] uses: no extra headers,
/// certificate verification on, no client certificate, and the app's own [`default_user_agent`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectionOptions {
    /// Headers attached to every request, on top of the generated client's own. The
    /// `authorization` header is rejected at mint time — it would clobber the bearer token
    /// (callers validate this earlier, where a user can be told; see `abs-core`'s save-time
    /// validation).
    pub extra_headers: Vec<(String, String)>,
    /// Skip certificate verification entirely — for self-hosted servers behind self-signed
    /// certificates. Applies to HTTPS traffic only.
    pub disable_ssl_verify: bool,
    /// Path to a PKCS#12 (`#12`/`.pfx`) bundle holding the client certificate + private key,
    /// for servers requiring mTLS, decrypted with [`ConnectionOptions::client_cert_password`].
    /// PKCS#12 rather than a combined PEM file because this workspace's TLS backend is
    /// native-tls (reqwest default features), where reqwest offers only `from_pkcs12_der` and
    /// `from_pkcs8_pem` — and the latter rejects the widespread `BEGIN RSA PRIVATE KEY`
    /// (PKCS#1) key encoding (native-tls's own `from_pkcs8_rejects_rsa_key` test), so a
    /// hand-rolled PEM splitter would silently fail on the most common real-world key format.
    /// Read at mint time (not baked into the builder earlier) so a file that has changed or
    /// disappeared on disk is a mint-time error, never a silent fallback.
    pub client_cert_path: Option<std::path::PathBuf>,
    /// The PKCS#12 bundle's export password, if it has one.
    pub client_cert_password: Option<String>,
    /// Replaces [`default_user_agent`] when set.
    pub user_agent: Option<String>,
}

/// The `User-Agent` every request sends unless a server's settings override it, so the app is
/// recognizable in a server's logs: `Audiobooklet/<version> (Linux <kernel>)`. Deliberately not
/// "Audiobookshelf": the project asks third-party clients not to use its name in a way that
/// suggests affiliation
/// (https://audiobookshelf.org/docs/faq/app#i-want-to-build-a-client-app-what-are-the-rules).
///
/// The kernel version is there for the server's device list, which takes the OS name and version
/// from this header alone (`DeviceInfo.setData`, via its bundled ua-parser-js 0.7.31): `(Linux)`
/// showed as "Linux undefined". The kernel works on every distribution and inside the Flatpak;
/// distribution names don't (the parser knows Debian or Ubuntu, but not PureOS or postmarketOS,
/// and an unknown one in front of "Linux" made it read the next word as the version). Without a
/// readable kernel version it's `(Linux)` as before. Worked out once per run.
pub fn default_user_agent() -> &'static str {
    static USER_AGENT: OnceLock<String> = OnceLock::new();
    USER_AGENT.get_or_init(|| {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok().and_then(|release| kernel_version(&release));
        user_agent_for(kernel.as_deref())
    })
}

fn user_agent_for(kernel: Option<&str>) -> String {
    match kernel {
        Some(kernel) => format!("Audiobooklet/{} (Linux {kernel})", env!("CARGO_PKG_VERSION")),
        None => format!("Audiobooklet/{} (Linux)", env!("CARGO_PKG_VERSION")),
    }
}

/// The numeric part of a kernel release (`6.6.101-librem5` → `6.6.101`): digits and dots only,
/// so nothing odd can reach the header or the server's parser. `None` when it doesn't start with
/// a number.
fn kernel_version(release: &str) -> Option<String> {
    let numeric: String = release.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').take(32).collect();
    let numeric = numeric.trim_end_matches('.');
    numeric.starts_with(|c: char| c.is_ascii_digit()).then(|| numeric.to_string())
}

/// Why minting a [`Client`] with connection options failed. Unlike the plain
/// headers/timeout-only constructors (which cannot fail beyond an invalid token), honoring a
/// server's full connection settings touches the filesystem (client certificate) and the TLS
/// backend, so failure is a normal, reportable outcome — surfaced through the same error paths
/// as any other network failure, never silently ignored.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionBuildError {
    #[error("access token contains characters that aren't valid in an HTTP header")]
    InvalidBearerToken,
    #[error("invalid custom header {0:?}: not a valid HTTP header name or value")]
    InvalidHeader(String),
    #[error("couldn't load the client certificate from {path:?}: {source}")]
    ClientCertificate {
        path: std::path::PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("couldn't build the HTTP client: {0}")]
    Build(reqwest::Error),
}

/// Reads and parses a PKCS#12 client-certificate bundle exactly as minting would, discarding
/// the loaded identity. The single source of truth for "is this client certificate usable" —
/// used by the mint itself and by save-time validation (`abs-core`), so the two can never
/// disagree about what counts as valid.
pub fn validate_client_cert_file(
    path: &std::path::Path,
    password: Option<&str>,
) -> Result<(), ConnectionBuildError> {
    load_client_identity(path, password).map(|_| ())
}

/// Whether `name` is a valid HTTP header name — exactly what minting would accept. Lets
/// save-time validation (`abs-core`) apply the real rule instead of a hand-rolled copy that
/// could drift from the mint's.
pub fn is_valid_header_name(name: &str) -> bool {
    reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok()
}

/// Whether `value` is a valid HTTP header value — see [`is_valid_header_name`].
pub fn is_valid_header_value(value: &str) -> bool {
    reqwest::header::HeaderValue::from_str(value).is_ok()
}

fn load_client_identity(
    path: &std::path::Path,
    password: Option<&str>,
) -> Result<reqwest::Identity, ConnectionBuildError> {
    let der = std::fs::read(path).map_err(|source| ConnectionBuildError::ClientCertificate {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    reqwest::Identity::from_pkcs12_der(&der, password.unwrap_or(""))
        .map_err(|source| ConnectionBuildError::ClientCertificate {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
}

/// The vendored spec declares `BearerAuth` as a security scheme on its authenticated operations,
/// but `progenitor` doesn't generate a per-call header parameter for it — confirmed by grepping
/// the generated `client.rs` for "Authorization"/"bearer" (no matches at all). So every
/// authenticated call (everything except `login`) needs a client built with one of the
/// `with_bearer_token*` constructors rather than the plain `Client::new`, which sends no auth at
/// all and gets `401`s back (caught live against `https://audiobooks.dev/audiobookshelf`, not
/// just in theory).
impl Client {
    /// An unauthenticated client with default [`ConnectionOptions`] — for the calls that run
    /// before any server settings exist (the first login) or when they can't be loaded. Unlike
    /// the generated `Client::new`, it sends [`default_user_agent`].
    pub fn with_default_options(baseurl: &str) -> Self {
        Self::with_options(baseurl, &ConnectionOptions::default()).expect("a client with default connection options always builds")
    }

    /// Builds a client that attaches `Authorization: Bearer <token>` to every request it sends.
    /// `token` is normally `LoginResult::access_token` fresh from `login`, or an already-persisted
    /// account's stored token. Uses a 15s connect/request timeout — long enough to tolerate a slow
    /// mobile connection, short enough that a call never hangs indefinitely when the server or
    /// network is simply gone (offline, airplane mode, etc.).
    pub fn with_bearer_token(baseurl: &str, token: &str) -> Result<Self, ConnectionBuildError> {
        Self::with_bearer_token_and_timeout(baseurl, token, std::time::Duration::from_secs(15))
    }

    /// Same as [`Client::with_bearer_token`], with a caller-chosen timeout instead of the default
    /// 15s. For calls on a critical path (e.g. resolving a playable URL) the default is
    /// appropriate; for best-effort background work (e.g. reconciling progress against the
    /// server before falling back to what's already stored locally) a much shorter timeout keeps
    /// a bad connection from being felt as a hang.
    pub fn with_bearer_token_and_timeout(
        baseurl: &str,
        token: &str,
        timeout: std::time::Duration,
    ) -> Result<Self, ConnectionBuildError> {
        Self::with_bearer_token_and_options(baseurl, token, timeout, &ConnectionOptions::default())
    }

    /// The full mint: bearer token plus a server's connection options. Every other
    /// `with_bearer_token*` constructor delegates here with default options — this is the one
    /// place reqwest primitives are assembled (headers, TLS behavior, identity, user agent).
    pub fn with_bearer_token_and_options(
        baseurl: &str,
        token: &str,
        timeout: std::time::Duration,
        options: &ConnectionOptions,
    ) -> Result<Self, ConnectionBuildError> {
        Self::with_options_and_headers(baseurl, options, Some(token), timeout)
    }

    /// An unauthenticated client with a server's connection options — for the one call that runs
    /// before any token exists but must still honor the connection's transport settings (token
    /// refresh, which carries the refresh token in a header).
    pub fn with_options(baseurl: &str, options: &ConnectionOptions) -> Result<Self, ConnectionBuildError> {
        Self::with_options_and_headers(baseurl, options, None, std::time::Duration::from_secs(15))
    }

    fn with_options_and_headers(
        baseurl: &str,
        options: &ConnectionOptions,
        bearer_token: Option<&str>,
        timeout: std::time::Duration,
    ) -> Result<Self, ConnectionBuildError> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(token) = bearer_token {
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| ConnectionBuildError::InvalidBearerToken)?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        for (name, value) in &options.extra_headers {
            // `HeaderMap::insert` would silently *replace* the bearer token — caught by test.
            // Callers reject this at save time (where a user can be told); this is the last
            // line of defense for settings that bypassed that check (hand-edited DB).
            if name.eq_ignore_ascii_case("authorization") {
                return Err(ConnectionBuildError::InvalidHeader(name.clone()));
            }
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ConnectionBuildError::InvalidHeader(name.clone()))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| ConnectionBuildError::InvalidHeader(name.to_string()))?;
            headers.insert(name, value);
        }

        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(timeout)
            .timeout(timeout);
        if options.disable_ssl_verify {
            builder = builder.danger_accept_invalid_certs(true);
        }
        builder = builder.user_agent(options.user_agent.as_deref().unwrap_or(default_user_agent()));
        if let Some(path) = &options.client_cert_path {
            builder = builder.identity(load_client_identity(path, options.client_cert_password.as_deref())?);
        }

        let http = builder.build().map_err(ConnectionBuildError::Build)?;
        Ok(Self::new_with_client(baseurl, http))
    }
}

#[derive(Debug, Deserialize)]
struct LibrarySeriesResponseBody {
    results: Vec<RawLibrarySeries>,
}

#[derive(Debug, Deserialize)]
struct RawLibrarySeries {
    id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    books: Vec<RawSeriesBook>,
}

#[derive(Debug, Deserialize)]
struct RawSeriesBook {
    id: Option<String>,
    /// The book's position within this one series — a nullable **string** on the wire (e.g.
    /// `"5"`, or `"5.5"` for an inserted novella), not a number; Audiobookshelf's own schema
    /// (`third_party/audiobookshelf-openapi/openapi.json`'s `sequence` schema) documents it this
    /// way, so this is passed through as-is rather than parsed into an integer.
    sequence: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesBookSummary {
    pub item_id: String,
    pub sequence: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LibrarySeriesSummary {
    pub id: String,
    pub name: String,
    pub books: Vec<SeriesBookSummary>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LibraryItemSummary {
    pub id: String,
    pub title: String,
    pub author: Option<String>,
    pub narrator: Option<String>,
    pub description: Option<String>,
    pub duration_seconds: f64,
    /// Milliseconds since the Unix epoch, as the server sends it — left as a raw `i64` here (not
    /// `chrono::DateTime`) so this crate doesn't need to pick a time-handling dependency just for
    /// one field; callers (`abs-core`) already depend on `chrono` for the storage layer's models
    /// and can convert.
    pub added_at_ms: i64,
    pub series_name: Option<String>,
    pub genres: Vec<String>,
    /// Whether the item has audio this app can play: false for podcasts (episodes aren't
    /// supported) and e-book-only books.
    pub has_audio: bool,
    /// What the list returns beyond the basics, each `None`/empty when the server didn't send it
    /// (or sent something of the wrong type): see `abs_storage::models::ItemExtras`.
    pub subtitle: Option<String>,
    pub size_bytes: Option<u64>,
    pub num_chapters: Option<i64>,
    pub tags: Vec<String>,
    pub published_year: Option<String>,
    pub publisher: Option<String>,
    pub language: Option<String>,
    pub explicit: Option<bool>,
    pub isbn: Option<String>,
    pub asin: Option<String>,
    /// `Some(false)` only when the server says the item has no cover; `None` when it didn't say.
    pub has_cover: Option<bool>,
    pub title_sort: Option<String>,
    pub author_sort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ItemDetailResponseBody {
    media: Option<RawItemMedia>,
}

#[derive(Debug, Deserialize)]
struct RawItemMedia {
    #[serde(rename = "audioFiles", default)]
    audio_files: Vec<RawAudioFile>,
    #[serde(default)]
    chapters: Vec<RawChapter>,
}

#[derive(Debug, Deserialize)]
struct RawAudioFile {
    ino: Option<String>,
    duration: Option<f64>,
    /// The server wraps the real file facts (path, byte size, timestamps) in a `metadata` object —
    /// only the byte size matters here (it's what download size estimates are computed from), and
    /// it's absent on some endpoints/servers, so everything else is ignored.
    metadata: Option<RawFileMetadata>,
}

#[derive(Debug, Deserialize)]
struct RawFileMetadata {
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawChapter {
    title: Option<String>,
    start: Option<f64>,
    end: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioFileRef {
    pub ino: String,
    pub duration_seconds: f64,
    /// The file's byte size as the server reports it in its metadata — `None` when the metadata
    /// is missing or omits it. Callers (download size estimates) treat `None` as "unknown",
    /// never as an error or as zero.
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChapterRef {
    pub title: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemPlaybackInfo {
    pub audio_files: Vec<AudioFileRef>,
    pub chapters: Vec<ChapterRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CoverBytes {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

/// A cover response's type; the server sends whatever the source file is (webp, jpeg, png).
fn cover_content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_string()
}

/// Who is sending local sessions, as `POST /api/session/local-all` reads it from the body
/// (`DeviceInfo.setData` in the server). The server keys its device records on `device_id`:
/// without one it inserts a new device row for every request.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub device_id: String,
    pub client_name: String,
    pub client_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One listening session recorded on this device, in the shape the server's
/// `PlaybackSessionManager.syncLocalSession` takes. Chapters and media metadata are left out on
/// purpose: the server fills them in from the item for a new session, and they'd be most of the
/// bytes. Times are milliseconds on the server's clock; positions and durations are seconds.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSession {
    pub id: String,
    pub library_item_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_id: Option<String>,
    pub media_type: &'static str,
    /// Audiobookshelf's `PlayMethod`: 0 direct play (streamed), 3 local (a downloaded file).
    pub play_method: i64,
    pub media_player: &'static str,
    pub display_title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_author: Option<String>,
    pub duration: f64,
    pub start_time: f64,
    pub current_time: f64,
    pub time_listening: f64,
    pub started_at: i64,
    pub updated_at: i64,
}

/// What the server did with one session of a `local-all` batch.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSessionResult {
    pub id: String,
    pub success: bool,
    /// Whether the session's position became the book's progress. `false` when the server
    /// already had progress updated after the session's `updatedAt` — from another device, or a
    /// deliberate edit — and kept that instead.
    #[serde(default)]
    pub progress_synced: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl LocalSessionResult {
    /// The server doesn't have the book (any more): the session can never be stored there. Only
    /// the server's own "Media item not found" — its "Library not found" is a server-side
    /// inconsistency that may clear up, not a missing book.
    pub fn is_item_missing(&self) -> bool {
        !self.success && self.error.as_deref().is_some_and(|error| error.eq_ignore_ascii_case("Media item not found"))
    }
}

/// The answer to a `local-all` batch: one result per session sent, and the server's clock as of
/// the reply (its `Date` header), for estimating how far this device's clock is off.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalSessionsSync {
    pub results: Vec<LocalSessionResult>,
    pub server_date: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalSessionsRequest<'a> {
    device_info: &'a DeviceInfo,
    sessions: &'a [LocalSession],
}

#[derive(Deserialize)]
struct LocalSessionsResponseBody {
    #[serde(default)]
    results: Vec<LocalSessionResult>,
}

/// A response's `Date` header, the server's clock when it answered (one-second resolution).
pub fn server_date(headers: &reqwest::header::HeaderMap) -> Option<chrono::DateTime<chrono::Utc>> {
    let value = headers.get(reqwest::header::DATE)?.to_str().ok()?;
    chrono::DateTime::parse_from_rfc2822(value).ok().map(|date| date.with_timezone(&chrono::Utc))
}

#[derive(Debug, Serialize)]
struct UpdateProgressRequest {
    #[serde(rename = "currentTime")]
    current_time: f64,
    duration: f64,
    #[serde(rename = "isFinished")]
    is_finished: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerProgress {
    pub library_item_id: String,
    pub current_time_seconds: f64,
    pub duration_seconds: f64,
    pub is_finished: bool,
    /// Milliseconds since the Unix epoch — the server's own field is a plain JS timestamp, not an
    /// RFC3339 string, so callers reconciling against `abs_storage`'s `DateTime<Utc>` need to
    /// convert it themselves (`chrono::DateTime::from_timestamp_millis`).
    pub last_update_ms: i64,
}

#[derive(Debug, Deserialize)]
struct RawMediaProgress {
    #[serde(rename = "libraryItemId")]
    library_item_id: Option<String>,
    #[serde(rename = "currentTime")]
    current_time: Option<f64>,
    duration: Option<f64>,
    #[serde(rename = "isFinished")]
    is_finished: Option<bool>,
    #[serde(rename = "lastUpdate")]
    last_update: Option<i64>,
}

impl RawMediaProgress {
    fn into_server_progress(self) -> Option<ServerProgress> {
        Some(ServerProgress {
            library_item_id: self.library_item_id?,
            current_time_seconds: self.current_time.unwrap_or(0.0),
            duration_seconds: self.duration.unwrap_or(0.0),
            is_finished: self.is_finished.unwrap_or(false),
            last_update_ms: self.last_update.unwrap_or(0),
        })
    }
}

#[derive(Debug, Deserialize)]
struct MeResponseBody {
    #[serde(rename = "mediaProgress", default)]
    media_progress: Vec<RawMediaProgress>,
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryItemsError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("the server rejected this session's credentials (HTTP {0})")]
    Unauthorized(u16),
    #[error("server returned an unexpected response: {0}")]
    UnexpectedResponse(String),
    /// The server answered 404: what was asked for doesn't exist there — for a cover, an item
    /// that simply has none. Told apart so a caller can remember that instead of asking again.
    #[error("not found: {0}")]
    NotFound(String),
}

/// Whether a transport failure means the server never answered at all — refused, unreachable,
/// a name that doesn't resolve, or no reply in time — as opposed to an answer the app couldn't
/// use. A certificate failure is excluded: the server is there, it just isn't trusted, which
/// needs a different message.
pub fn is_no_answer(err: &reqwest::Error) -> bool {
    err.status().is_none() && !is_tls_error(err) && (err.is_connect() || err.is_timeout() || err.is_request())
}

/// [`is_no_answer`] for a generated-client call's error.
pub fn is_unreachable_error<E>(err: &crate::Error<E>) -> bool {
    matches!(err, crate::Error::CommunicationError(e) if is_no_answer(e))
}

impl LibraryItemsError {
    /// Whether the server never answered (see [`is_no_answer`]).
    pub fn is_unreachable(&self) -> bool {
        matches!(self, LibraryItemsError::Network(e) if is_no_answer(e))
    }

    /// What happened, in one line for logs: a transport failure ([`LibraryItemsError::Network`])
    /// gets a short classification ([`transport_error_kind`]) plus the full `source()` chain
    /// ([`error_chain`]) — reqwest's Display alone is identical for a timeout, a DNS failure
    /// and a certificate error — while the other variants already name their cause in
    /// `Display` (an HTTP status, in particular, e.g. a 404 from a missing cover).
    pub fn details(&self) -> String {
        match self {
            LibraryItemsError::Network(err) => format!("{}: {}", transport_error_kind(err), error_chain(err)),
            other => other.to_string(),
        }
    }
}

/// Whether an HTTP status means "this session itself is dead — only signing in again fixes it".
/// 401 is the obvious one; 403 is included because Audiobookshelf revokes access per-user and a
/// demoted/removed user gets 403s with perfectly valid tokens, which the UI should treat the
/// same way (sign in again, possibly as someone else) rather than as a transient sync hiccup.
pub fn is_auth_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN
}

/// A dedicated error type for [`Client::get_item_file_response`] rather than reusing
/// [`LibraryItemsError`] — a download pipeline needs to tell a 4xx (bad request/auth/not found,
/// never worth retrying) apart from a 5xx or a network error (worth retrying with backoff), which
/// `LibraryItemsError::UnexpectedResponse`'s plain `String` throws away.
#[derive(Debug, thiserror::Error)]
pub enum TrackFileError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("server returned HTTP {0}")]
    Status(reqwest::StatusCode),
}

impl TrackFileError {
    /// A 4xx means the request itself is wrong (bad token, item/track no longer exists) — retrying
    /// the exact same request will never succeed, unlike a `5xx`/network blip.
    pub fn is_retryable(&self) -> bool {
        match self {
            TrackFileError::Network(_) => true,
            TrackFileError::Status(status) => !status.is_client_error(),
        }
    }
}

/// A track's audio file response, ready to be streamed to disk. See
/// [`Client::get_item_file_response`] for why this exposes the live [`reqwest::Response`] instead
/// of pre-collecting its body.
#[derive(Debug)]
pub struct TrackFileResponse {
    pub resumed: bool,
    pub total_size: Option<u64>,
    pub content_type: Option<String>,
    pub response: reqwest::Response,
}

impl Client {
    /// Fetch a library's items with their media metadata (title, author, narrator, description,
    /// duration). Hand-written rather than using the generated `get_library_items`: that call's
    /// response type (`GetLibraryItemsResponse::results: Vec<LibraryItemBase>`) has no `media`
    /// field at all — a real gap in the vendored spec's schema for this endpoint, confirmed by
    /// checking `LibraryItemBase`'s generated fields and by hitting the real server directly (see
    /// `third_party/audiobookshelf-openapi/README.md`'s "Known gaps"): it always sends
    /// `media.metadata.{title,authorName,narratorName,description}` and `media.duration`, the
    /// generated type just silently drops them on deserialize. Items missing an id, title, or the
    /// whole `media`/`metadata` object are skipped rather than erroring the whole page — a
    /// malformed entry shouldn't take down every other item in the library.
    pub async fn get_library_items_with_media(
        &self,
        library_id: &str,
    ) -> Result<Vec<LibraryItemSummary>, LibraryItemsError> {
        let response = self
            .client()
            .get(format!("{}/api/libraries/{library_id}/items", self.baseurl()))
            .send()
            .await?;

        if is_auth_status(response.status()) {
            return Err(LibraryItemsError::Unauthorized(response.status().as_u16()));
        }
        if !response.status().is_success() {
            return Err(LibraryItemsError::UnexpectedResponse(format!(
                "GET /api/libraries/{library_id}/items returned HTTP {}",
                response.status()
            )));
        }

        let body: LibraryItemsResponseBody = response.json().await?;

        Ok(body
            .results
            .into_iter()
            .filter_map(|item| {
                let id = item.id?;
                let media = item.media?;
                let has_audio = item.media_type.as_deref() != Some("podcast") && media.has_audio();
                let metadata = media.metadata?;
                let title = metadata.title?;
                Some(LibraryItemSummary {
                    id,
                    title,
                    author: non_blank(metadata.author_name),
                    narrator: non_blank(metadata.narrator_name),
                    description: metadata.description,
                    duration_seconds: media.duration.unwrap_or(0.0),
                    added_at_ms: item.added_at.unwrap_or(0),
                    series_name: non_blank(metadata.series_name),
                    genres: metadata.genres,
                    has_audio,
                    subtitle: non_blank(metadata.subtitle),
                    size_bytes: media.size,
                    num_chapters: media.num_chapters.filter(|count| *count >= 0),
                    tags: media.tags.unwrap_or_default().into_iter().filter(|tag| !tag.trim().is_empty()).collect(),
                    published_year: non_blank(metadata.published_year),
                    publisher: non_blank(metadata.publisher),
                    language: non_blank(metadata.language),
                    explicit: metadata.explicit,
                    isbn: non_blank(metadata.isbn),
                    asin: non_blank(metadata.asin),
                    has_cover: media.cover_path.map(|path| path.is_some_and(|path| !path.trim().is_empty())),
                    title_sort: non_blank(metadata.title_sort),
                    author_sort: non_blank(metadata.author_sort),
                })
            })
            .collect())
    }

    /// Fetch a library's series list, each with its books and their per-series `sequence` (e.g.
    /// `"5"`, or `"5.5"` for an inserted novella) — the only place Audiobookshelf's API actually
    /// exposes a book's position within a series; `media.metadata.seriesName` (used above) is a
    /// flat convenience string with no number attached.
    ///
    /// Named `_summaries` rather than plain `get_library_series` — that name is already taken by
    /// the *generated* client method (this endpoint isn't an outright gap like
    /// `get_library_items_with_media`'s). This hand-written version exists anyway: the generated
    /// response types (`SeriesId`, `LibraryItemId`) require every id to be a syntactically valid
    /// UUID, deserializing the *whole* response as an error otherwise — inconsistent with how
    /// this crate treats item/series ids everywhere else, as opaque strings (see
    /// `LibraryItemSummary.id: String` above), and needlessly fragile against a server that ever
    /// sends a non-UUID id. This mirrors `get_library_items_with_media`'s own `filter_map`
    /// posture instead: a series or book entry missing its id is skipped rather than failing the
    /// whole page. `GET /api/libraries/{id}/series` is unpaginated here too, same as
    /// `get_library_items_with_media` — no `limit`/`page` params sent, relying on the server
    /// returning everything in one response.
    pub async fn get_library_series_summaries(&self, library_id: &str) -> Result<Vec<LibrarySeriesSummary>, LibraryItemsError> {
        let response = self.client().get(format!("{}/api/libraries/{library_id}/series", self.baseurl())).send().await?;

        if is_auth_status(response.status()) {
            return Err(LibraryItemsError::Unauthorized(response.status().as_u16()));
        }
        if !response.status().is_success() {
            return Err(LibraryItemsError::UnexpectedResponse(format!(
                "GET /api/libraries/{library_id}/series returned HTTP {}",
                response.status()
            )));
        }

        let body: LibrarySeriesResponseBody = response.json().await?;

        Ok(body
            .results
            .into_iter()
            .filter_map(|series| {
                let id = series.id?;
                let name = series.name?;
                let books = series
                    .books
                    .into_iter()
                    .filter_map(|book| Some(SeriesBookSummary { item_id: book.id?, sequence: book.sequence }))
                    .collect();
                Some(LibrarySeriesSummary { id, name, books })
            })
            .collect())
    }

    /// Fetch the audio file(s) backing an item, for resolving a playable URL. Hand-written: the
    /// vendored spec has **zero** `/api/items/*` coverage at all (see
    /// `third_party/audiobookshelf-openapi/README.md`'s "Known gaps") — there is no generated
    /// method to call here. Confirmed live against `https://audiobooks.dev/audiobookshelf` that
    /// `GET /api/items/:id` returns `media.audioFiles[].{ino, duration}`, and that
    /// `GET /api/items/:id/file/:ino?token=<access_token>` streams the actual audio bytes — so
    /// this only needs to parse the `ino`/`duration` pair per file; the actual streaming URL is
    /// assembled by the caller (`abs_core::streaming::resolve_stream_target`), not here.
    pub async fn get_item_playback_info(&self, item_id: &str) -> Result<ItemPlaybackInfo, LibraryItemsError> {
        let response = self.client().get(format!("{}/api/items/{item_id}", self.baseurl())).send().await?;

        if is_auth_status(response.status()) {
            return Err(LibraryItemsError::Unauthorized(response.status().as_u16()));
        }
        if !response.status().is_success() {
            return Err(LibraryItemsError::UnexpectedResponse(format!(
                "GET /api/items/{item_id} returned HTTP {}",
                response.status()
            )));
        }

        let body: ItemDetailResponseBody = response.json().await?;
        let (audio_files, chapters) = match body.media {
            Some(media) => (media.audio_files, media.chapters),
            None => (Vec::new(), Vec::new()),
        };
    let audio_files = audio_files
        .into_iter()
        .filter_map(|f| {
            Some(AudioFileRef { ino: f.ino?, duration_seconds: f.duration.unwrap_or(0.0), size_bytes: f.metadata.and_then(|m| m.size) })
        })
        .collect();
        // A malformed chapter entry (missing title/start/end) is skipped rather than failing the
        // whole call, same tolerance as audio files above — one bad chapter shouldn't take down
        // playback or the chapters sheet for the rest of the book.
        let chapters = chapters
            .into_iter()
            .filter_map(|c| Some(ChapterRef { title: c.title?, start_seconds: c.start?, end_seconds: c.end? }))
            .collect();

        Ok(ItemPlaybackInfo { audio_files, chapters })
    }

    /// Fetch an item's cover image. Hand-written, same `/api/items/*` gap as
    /// `get_item_playback_info`. Confirmed live: `GET /api/items/:id/cover` with the bearer-token
    /// client (no `?token=` query param needed, unlike `/file/:ino`) returns `200` and the raw
    /// image bytes; the content-type varies by library (`image/webp` for the one checked live,
    /// but Audiobookshelf may return jpeg/png depending on the source cover file), so callers must
    /// not assume a fixed extension.
    pub async fn get_item_cover(&self, item_id: &str) -> Result<CoverBytes, LibraryItemsError> {
        let response = self.cover_response(item_id, false).await?;
        let content_type = cover_content_type(&response);
        let bytes = response.bytes().await?.to_vec();
        Ok(CoverBytes { bytes, content_type })
    }

    /// The cover as it was uploaded to the server (`?raw=1`), not the resized copy (400 px wide)
    /// that a plain `GET /api/items/:id/cover` returns — for showing the cover full-screen.
    /// Streamed, with `on_progress(received, total)` after every chunk (`total` when the server
    /// sends a length), since an original can be several MB on a slow connection.
    pub async fn get_item_cover_original(
        &self,
        item_id: &str,
        on_progress: impl Fn(u64, Option<u64>),
    ) -> Result<CoverBytes, LibraryItemsError> {
        let mut response = self.cover_response(item_id, true).await?;
        let content_type = cover_content_type(&response);
        let total = response.content_length();
        let mut bytes = Vec::with_capacity(total.unwrap_or(0).min(64 * 1024 * 1024) as usize);
        on_progress(0, total);
        while let Some(chunk) = response.chunk().await? {
            bytes.extend_from_slice(&chunk);
            on_progress(bytes.len() as u64, total);
        }
        Ok(CoverBytes { bytes, content_type })
    }

    async fn cover_response(&self, item_id: &str, original: bool) -> Result<reqwest::Response, LibraryItemsError> {
        let query = if original { "?raw=1" } else { "" };
        let response = self.client().get(format!("{}/api/items/{item_id}/cover{query}", self.baseurl())).send().await?;

        if is_auth_status(response.status()) {
            return Err(LibraryItemsError::Unauthorized(response.status().as_u16()));
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(LibraryItemsError::NotFound(format!("GET /api/items/{item_id}/cover{query} returned HTTP {}", response.status())));
        }
        if !response.status().is_success() {
            return Err(LibraryItemsError::UnexpectedResponse(format!(
                "GET /api/items/{item_id}/cover{query} returned HTTP {}",
                response.status()
            )));
        }
        Ok(response)
    }

    /// Fetch a track's audio file, optionally resuming with an HTTP `Range` request — the raw
    /// per-track byte source a download pipeline streams to disk (as opposed to `StreamTarget`'s
    /// URL, which is handed to GStreamer for playback and never fetched as bytes in Rust). Same
    /// `/api/items/*` gap as the other hand-written extensions above; confirmed live: the file
    /// endpoint sends `accept-ranges: bytes` and honors `Range: bytes=N-` with a `206` and a
    /// matching `Content-Range`, exactly like a normal HTTP file server.
    ///
    /// Returns the live `reqwest::Response` unconsumed (not `.bytes()`-collected) so the caller can
    /// stream it in chunks — collecting a whole audiobook track into memory before writing it out
    /// would defeat the point of a resumable, progress-reporting download. `resumed` distinguishes
    /// a server that actually honored the `Range` request (`206`) from one that ignored it and sent
    /// the full body from byte 0 (`200`) — callers must check this rather than assuming a `Some`
    /// `range_start` was respected, since blindly appending a full-body response onto existing
    /// partial bytes would corrupt the file. `total_size` is always the *whole file's* size (parsed
    /// from `Content-Range`'s `.../total` suffix when resumed, since a `206`'s `Content-Length` is
    /// only the remaining-bytes count, not the file's total size) — never the size of just this
    /// response's body, so callers can report accurate progress against the true track size
    /// regardless of where a resume started from.
    pub async fn get_item_file_response(&self, item_id: &str, ino: &str, range_start: Option<u64>) -> Result<TrackFileResponse, TrackFileError> {
        let mut request = self.client().get(format!("{}/api/items/{item_id}/file/{ino}", self.baseurl()));
        if let Some(start) = range_start {
            request = request.header(reqwest::header::RANGE, format!("bytes={start}-"));
        }
        let response = request.send().await?;

        if !response.status().is_success() {
            return Err(TrackFileError::Status(response.status()));
        }

        let resumed = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let total_size = if resumed {
            response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.rsplit('/').next())
                .and_then(|s| s.parse::<u64>().ok())
        } else {
            response.content_length()
        };
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);

        Ok(TrackFileResponse { resumed, total_size, content_type, response })
    }

    /// Push local playback progress up to the server, so it shows up in the official apps and
    /// survives a fresh install. Hand-written: `/api/me/*` has zero coverage in the vendored spec
    /// (see `third_party/audiobookshelf-openapi/README.md`'s "Known gaps"). Confirmed live against
    /// `https://audiobooks.dev/audiobookshelf`: `PATCH /api/me/progress/:libraryItemId` with a
    /// JSON body of `{currentTime, duration, isFinished}` returns `200 OK` and the change shows up
    /// in a subsequent `GET /api/me`'s `mediaProgress`; a nonexistent item id returns `404`. The
    /// server computes its own `progress` fraction — passing one had no visible effect — so this
    /// only sends the fields that actually matter.
    pub async fn update_media_progress(
        &self,
        item_id: &str,
        current_time_seconds: f64,
        duration_seconds: f64,
        is_finished: bool,
    ) -> Result<(), LibraryItemsError> {
        let response = self
            .client()
            .patch(format!("{}/api/me/progress/{item_id}", self.baseurl()))
            .json(&UpdateProgressRequest { current_time: current_time_seconds, duration: duration_seconds, is_finished })
            .send()
            .await?;

        if is_auth_status(response.status()) {
            return Err(LibraryItemsError::Unauthorized(response.status().as_u16()));
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(LibraryItemsError::NotFound(format!("item {item_id}")));
        }
        if !response.status().is_success() {
            return Err(LibraryItemsError::UnexpectedResponse(format!(
                "PATCH /api/me/progress/{item_id} returned HTTP {}",
                response.status()
            )));
        }
        Ok(())
    }

    /// Sends listening sessions recorded on this device, in one request: `POST
    /// /api/session/local-all` with `{deviceInfo, sessions}`, answered with one result per
    /// session (`PlaybackSessionManager.syncLocalSessionsRequest`). Hand-written: the vendored
    /// spec has no `/session` paths. A server too old for it (or a proxy that blocks it) answers
    /// 404 or 405, reported as [`LibraryItemsError::NotFound`] so the caller can fall back to
    /// [`Client::update_media_progress`].
    pub async fn sync_local_sessions(&self, device_info: &DeviceInfo, sessions: &[LocalSession]) -> Result<LocalSessionsSync, LibraryItemsError> {
        let response = self
            .client()
            .post(format!("{}/api/session/local-all", self.baseurl()))
            .json(&LocalSessionsRequest { device_info, sessions })
            .send()
            .await?;

        if is_auth_status(response.status()) {
            return Err(LibraryItemsError::Unauthorized(response.status().as_u16()));
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND || response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(LibraryItemsError::NotFound("POST /api/session/local-all".to_string()));
        }
        if !response.status().is_success() {
            return Err(LibraryItemsError::UnexpectedResponse(format!(
                "POST /api/session/local-all returned HTTP {}",
                response.status()
            )));
        }
        let server_date = server_date(response.headers());
        let body: LocalSessionsResponseBody = response.json().await?;
        Ok(LocalSessionsSync { results: body.results, server_date })
    }

    /// Fetch this item's progress as the server currently has it, for reconciling against what's
    /// stored locally before resuming playback — so opening a book reflects progress made on
    /// another device or the official apps, not just this client's own last local write.
    /// Hand-written, same `/api/me/*` gap as `update_media_progress`. Confirmed live: `GET
    /// /api/me/progress/:libraryItemId` returns `200` with the progress object when one exists,
    /// and `404` when it doesn't — which is a normal, expected "no progress yet" outcome here, not
    /// an error.
    pub async fn get_media_progress(&self, item_id: &str) -> Result<Option<ServerProgress>, LibraryItemsError> {
        let response = self.client().get(format!("{}/api/me/progress/{item_id}", self.baseurl())).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if is_auth_status(response.status()) {
            return Err(LibraryItemsError::Unauthorized(response.status().as_u16()));
        }
        if !response.status().is_success() {
            return Err(LibraryItemsError::UnexpectedResponse(format!(
                "GET /api/me/progress/{item_id} returned HTTP {}",
                response.status()
            )));
        }

        let raw: RawMediaProgress = response.json().await?;
        Ok(raw.into_server_progress())
    }

    /// Fetch every item this account has progress on, in one call — for reconciling Home's
    /// "Continue Listening" shelf against the server without one request per item. Hand-written:
    /// `GET /api/me` isn't in the vendored spec either, and returns the full user object with a
    /// `mediaProgress` array alongside fields this client has no use for (permissions, accessible
    /// libraries, etc.), which are simply ignored here.
    pub async fn get_all_media_progress(&self) -> Result<Vec<ServerProgress>, LibraryItemsError> {
        let response = self.client().get(format!("{}/api/me", self.baseurl())).send().await?;

        if is_auth_status(response.status()) {
            return Err(LibraryItemsError::Unauthorized(response.status().as_u16()));
        }
        if !response.status().is_success() {
            return Err(LibraryItemsError::UnexpectedResponse(format!("GET /api/me returned HTTP {}", response.status())));
        }

        let body: MeResponseBody = response.json().await?;
        Ok(body.media_progress.into_iter().filter_map(RawMediaProgress::into_server_progress).collect())
    }

    /// Log in with a username and password, returning the tokens needed for subsequent
    /// authenticated requests. Not in the vendored spec at all (no auth endpoints are documented)
    /// — hand-written against the real route confirmed in the server source (`server/Auth.js`):
    /// `POST {baseurl}/login` with a JSON `{username, password}` body, passport-local under the
    /// hood. The `x-return-tokens: true` header is what makes it return the access/refresh tokens
    /// in the JSON body instead of only setting a cookie — required for any non-browser client,
    /// this one included.
    pub async fn login(&self, username: &str, password: &str) -> Result<LoginResult, LoginError> {
        let response = self
            .client()
            .post(format!("{}/login", self.baseurl()))
            .header("x-return-tokens", "true")
            .json(&LoginRequest { username, password })
            .send()
            .await
            .map_err(classify_transport_error)?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(LoginError::InvalidCredentials);
        }
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }

        let body: LoginResponseBody = response.json().await.map_err(classify_transport_error)?;
        let access_token = body.user.access_token.ok_or_else(|| {
            LoginError::UnexpectedResponse(
                "login succeeded but response carried no accessToken (was x-return-tokens sent?)"
                    .into(),
            )
        })?;

        Ok(LoginResult {
            user_id: body.user.id,
            username: body.user.username,
            access_token,
            refresh_token: body.user.refresh_token,
        })
    }

    /// Who this client's token belongs to (`GET /api/me`), for signing in with an API key: a key
    /// is a bearer token with no username attached, so the account row is named after what the
    /// server says. 401 means the token isn't accepted.
    pub async fn whoami(&self) -> Result<Whoami, LoginError> {
        let response = self.client().get(format!("{}/api/me", self.baseurl())).send().await.map_err(classify_transport_error)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(LoginError::InvalidCredentials);
        }
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        let body: WhoamiBody = response
            .json()
            .await
            .map_err(|err| if err.is_decode() { LoginError::UnexpectedResponse("GET /api/me didn't answer with a user".into()) } else { classify_transport_error(err) })?;
        Ok(Whoami { user_id: body.id, username: body.username })
    }

    /// Exchange a refresh token for a fresh token pair. Hand-written (no auth endpoints are in the
    /// vendored spec — same as `login`), against the JWT auth system's documented shape (server
    /// v2.26.0+, github.com/advplyr/audiobookshelf discussion #4460): `POST {baseurl}/auth/refresh`
    /// with the refresh token in the `x-refresh-token` header (mobile clients don't get cookies),
    /// responding with the same body shape as `/login`. The refresh token **rotates on every
    /// use** — the new pair from the response must replace the old one in storage, and a 401 here
    /// means the server no longer knows this session at all (expired, revoked, or the server lost
    /// its session store), so the only way back in is signing in again.
    ///
    /// The wait has two parts. The server gets [`REFRESH_ANSWER_TIMEOUT`] to start answering,
    /// like any call. Once it has answered with success, it has already swapped the pair and
    /// this reply holds the only copy of the new one, so the rest of it gets
    /// [`REFRESH_BODY_TIMEOUT`]. The reply is large (the server sends the whole user record,
    /// ~30 KB), and on a phone just out of sleep it can take longer than the answer did; giving
    /// up on it is what loses the login.
    pub async fn refresh(&self, refresh_token: &str) -> Result<LoginResult, LoginError> {
        self.refresh_within(refresh_token, REFRESH_ANSWER_TIMEOUT, REFRESH_BODY_TIMEOUT).await
    }

    async fn refresh_within(&self, refresh_token: &str, answer_timeout: std::time::Duration, body_timeout: std::time::Duration) -> Result<LoginResult, LoginError> {
        let request = self
            .client()
            .post(format!("{}/auth/refresh", self.baseurl()))
            .header("x-return-tokens", "true")
            .header("x-refresh-token", refresh_token)
            // Replaces the client's own limit for the whole request, which would cut the reply
            // off; the wait for the answer is limited separately below.
            .timeout(answer_timeout + body_timeout)
            .send();
        let response = tokio::time::timeout(answer_timeout, request)
            .await
            .map_err(|_| LoginError::NoAnswer)?
            .map_err(classify_transport_error)?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(LoginError::SessionExpired);
        }
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }

        let body: LoginResponseBody = response.json().await.map_err(classify_transport_error)?;
        let access_token = body.user.access_token.ok_or_else(|| {
            LoginError::UnexpectedResponse(
                "token refresh succeeded but response carried no accessToken (was x-return-tokens sent?)".into(),
            )
        })?;

        Ok(LoginResult {
            user_id: body.user.id,
            username: body.user.username,
            access_token,
            refresh_token: body.user.refresh_token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn with_bearer_token_sends_the_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .and(header("authorization", "Bearer secret-token-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "libraries": [] })))
            .mount(&server)
            .await;

        let client = Client::with_bearer_token(&server.uri(), "secret-token-123").unwrap();
        let response = client.get_libraries().await.unwrap();
        assert!(response.into_inner().libraries.is_empty());
    }

    /// A closed port refuses a connection immediately — it doesn't exercise the *timeout* at all,
    /// just normal error handling. This proves the timeout itself actually cuts off a request
    /// that's genuinely stuck (TCP connects fine, the server just never answers — closer to what
    /// "connectivity silently drops mid-request" looks like than an immediate refusal), so a real
    /// network failure can never hang the caller indefinitely regardless of what stage it's stuck
    /// at. Uses a 200ms timeout (not the 15s/5s a real caller would use) purely to keep this test
    /// fast; the mechanism being tested is the same.
    #[tokio::test]
    async fn with_bearer_token_and_timeout_does_not_hang_on_a_server_that_never_responds() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // Accept the connection and hold it open forever without writing a response.
            // The stream must be bound, not discarded — `let _ = accept()` drops it (and the
            // socket with it), closing the connection instantly and turning the client's
            // failure into "connection closed" instead of the timeout this exists to test.
            let (_stream, _) = listener.accept().expect("the test client should connect");
            std::thread::sleep(std::time::Duration::from_secs(60));
        });

        let client = Client::with_bearer_token_and_timeout(&format!("http://{addr}"), "token", std::time::Duration::from_millis(200)).unwrap();

        let started = std::time::Instant::now();
        let result = client.get_media_progress("item-1").await;
        assert!(result.is_err(), "a request to a server that never responds must time out, not succeed");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "the timeout should cut this off in ~200ms, not hang");
    }

    /// The full classification pipeline on a genuine timeout (same never-responding server shape
    /// as the hang test above): `LibraryItemsError::details` must lead with the kind — "timeout"
    /// — and walk the source chain, because reqwest's Display alone is identical for every
    /// failure mode and the log line built on this is what a user debugging a slow server sees.
    #[tokio::test]
    async fn library_items_error_details_leads_with_the_kind_and_walks_the_chain() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // Accept the connection and hold it open forever without writing a response.
            // The stream must be bound, not discarded — `let _ = accept()` drops it (and the
            // socket with it), closing the connection instantly and turning the client's
            // failure into "connection closed" instead of the timeout this exists to test.
            let (_stream, _) = listener.accept().expect("the test client should connect");
            std::thread::sleep(std::time::Duration::from_secs(60));
        });

        let client = Client::with_bearer_token_and_timeout(&format!("http://{addr}"), "token", std::time::Duration::from_millis(200)).unwrap();
        let err = client.get_library_items_with_media("lib-1").await.unwrap_err();

        let details = err.details();
        assert!(details.starts_with("timeout: "), "got {details}");
        assert!(details.contains("error sending request for url"), "the chain should include the display text: {details}");
    }

    /// A refused connection classifies as `"connect"` (DNS failures land there too, hyper
    /// folding them into the connect phase) — distinct from `"timeout"`, which the classification
    /// checks first.
    #[tokio::test]
    async fn transport_error_kind_classifies_a_refused_connection_as_connect() {
        // Port 1 on loopback is (practically always) closed — the OS refuses immediately.
        let client = Client::with_bearer_token_and_timeout("http://127.0.0.1:1", "token", std::time::Duration::from_secs(2)).unwrap();
        let err = client.get_library_items_with_media("lib-1").await.unwrap_err();

        let LibraryItemsError::Network(e) = &err else { panic!("expected a network error, got {err:?}") };
        assert_eq!(transport_error_kind(e), "connect", "got {}", err.details());
    }

    /// A non-transport failure passes through unchanged: its Display already names the cause
    /// (an HTTP status, in particular — a 404 from a missing cover, say).
    #[test]
    fn library_items_error_details_passthrough_for_non_transport_variants() {
        let err = LibraryItemsError::UnexpectedResponse("GET /x returned HTTP 404".to_string());
        assert_eq!(err.details(), err.to_string());
    }

    /// `error_chain` walks every source into one " → " line — the base all transport-error logs
    /// and "Show details" surfaces are built on.
    #[test]
    fn error_chain_walks_the_whole_source_chain() {
        #[derive(Debug, thiserror::Error)]
        #[error("root cause: {0}")]
        struct Root(&'static str);

        #[derive(Debug, thiserror::Error)]
        #[error("middle layer")]
        struct Middle(#[source] Root);

        assert_eq!(error_chain(&Middle(Root("disk full"))), "middle layer → root cause: disk full");
    }

    /// A server's custom headers and user-agent override must reach the actual wire — this is
    /// the whole point of the Connection page's Advanced settings (e.g. a reverse proxy keyed
    /// on a header). Mocked end-to-end: the mock only answers when both arrive.
    #[tokio::test]
    async fn custom_headers_and_user_agent_reach_the_wire() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .and(header("x-custom-header", "secret-value"))
            .and(header("user-agent", "MyAgent/1.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "libraries": [] })))
            .mount(&server)
            .await;

        let options = ConnectionOptions {
            extra_headers: vec![("X-Custom-Header".to_string(), "secret-value".to_string())],
            user_agent: Some("MyAgent/1.0".to_string()),
            ..ConnectionOptions::default()
        };
        let client = Client::with_bearer_token_and_options(&server.uri(), "token", std::time::Duration::from_secs(5), &options).unwrap();

        let response = client.get_libraries().await.expect("the mock matched custom header and user-agent");
        assert!(response.into_inner().libraries.is_empty());
    }

    /// Defense in depth behind `abs-core`'s save-time validation: even if an `authorization`
    /// custom header ever reached minting (hand-edited DB), it must be rejected rather than
    /// silently clobbering the bearer token the client exists to send.
    #[tokio::test]
    async fn an_authorization_custom_header_is_rejected_at_mint() {
        let options = ConnectionOptions {
            extra_headers: vec![("Authorization".to_string(), "Bearer stolen".to_string())],
            ..ConnectionOptions::default()
        };
        let err = Client::with_bearer_token_and_options("http://localhost:1", "token", std::time::Duration::from_secs(5), &options).unwrap_err();
        assert!(matches!(err, ConnectionBuildError::InvalidHeader(ref name) if name.eq_ignore_ascii_case("authorization")), "got {err:?}");
    }

    /// A configured client certificate that's missing from disk (deleted after save, unmounted
    /// path) is a mint-time error — never a silent fallback to a cert-less client that would
    /// then fail the request itself with a confusing server-side rejection.
    #[tokio::test]
    async fn a_missing_client_certificate_file_is_a_mint_error() {
        let options = ConnectionOptions {
            client_cert_path: Some(std::path::PathBuf::from("/nonexistent/cert.p12")),
            client_cert_password: Some("secret".to_string()),
            ..ConnectionOptions::default()
        };
        let err = Client::with_bearer_token_and_options("http://localhost:1", "token", std::time::Duration::from_secs(5), &options).unwrap_err();
        assert!(matches!(err, ConnectionBuildError::ClientCertificate { ref path, .. } if path == std::path::Path::new("/nonexistent/cert.p12")), "got {err:?}");
    }

    /// Certificate verification off must actually connect to a host whose certificate can't be
    /// verified — the exact scenario the Connection page's "Disable SSL verification" switch
    /// exists for (self-hosted servers behind self-signed certificates). `#[ignore]`d so the
    /// workspace suite stays hermetic; run explicitly with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn ssl_verification_disabled_connects_to_a_self_signed_host() {
        let options = ConnectionOptions { disable_ssl_verify: true, ..ConnectionOptions::default() };
        let client = Client::with_options("https://self-signed.badssl.com", &options).unwrap();

        let response = client
            .client()
            .get("https://self-signed.badssl.com")
            .send()
            .await
            .expect("with verification disabled, a self-signed certificate must not be a TLS error");
        let _ = response.status(); // any HTTP status at all proves the TLS handshake succeeded
    }

    /// The same host with verification on (the default) must fail the handshake — proving the
    /// test above is meaningful and not just "the network was down". `#[ignore]`d likewise.
    #[tokio::test]
    #[ignore]
    async fn ssl_verification_enabled_fails_on_a_self_signed_host() {
        let client = Client::with_options("https://self-signed.badssl.com", &ConnectionOptions::default()).unwrap();
        let result = client.client().get("https://self-signed.badssl.com").send().await;
        assert!(result.is_err(), "a self-signed certificate must be rejected by default");
    }

    #[tokio::test]
    async fn a_plain_client_sends_no_authorization_header_and_gets_rejected() {
        // Regression test: caught live against the real demo server, where `Client::new` (no
        // auth) got a 401 back from an authenticated endpoint. wiremock rejects the request here
        // for the same reason — no Authorization header was ever attached.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .and(header("authorization", "Bearer secret-token-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "libraries": [] })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let result = client.get_libraries().await;
        assert!(result.is_err(), "a request with no Authorization header should not match the mock");
    }

    #[tokio::test]
    async fn successful_login_returns_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .and(header("x-return-tokens", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": {
                    "id": "user-1",
                    "username": "jane",
                    "accessToken": "abc123",
                    "refreshToken": "refresh456",
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let result = client.login("jane", "hunter2").await.unwrap();

        assert_eq!(result.user_id, "user-1");
        assert_eq!(result.username, "jane");
        assert_eq!(result.access_token, "abc123");
        assert_eq!(result.refresh_token.as_deref(), Some("refresh456"));
    }

    #[tokio::test]
    async fn wrong_password_surfaces_as_invalid_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let err = client.login("jane", "wrong").await.unwrap_err();
        assert!(matches!(err, LoginError::InvalidCredentials));
    }

    /// What a login's unsuccessful HTTP answers become: a proxy's 403/407 is "blocked", nginx's
    /// 400 about a missing client certificate is a client-certificate problem, everything else
    /// is its status (a wrong path is 404, a server problem 5xx).
    #[tokio::test]
    async fn unsuccessful_login_statuses_are_told_apart() {
        async fn login_answering(status: u16, body: &str) -> LoginError {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/login"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            Client::new(&server.uri()).login("jane", "hunter2").await.unwrap_err()
        }

        assert!(matches!(login_answering(500, "").await, LoginError::HttpStatus(500)));
        assert!(matches!(login_answering(404, "<html>Not found</html>").await, LoginError::HttpStatus(404)));
        assert!(matches!(login_answering(403, "blocked by the proxy").await, LoginError::Blocked(403)));
        assert!(matches!(login_answering(407, "").await, LoginError::Blocked(407)));
        assert!(matches!(login_answering(400, "plain old bad request").await, LoginError::HttpStatus(400)));
        let nginx = "<html><head><title>400 No required SSL certificate was sent</title></head></html>";
        assert!(matches!(login_answering(400, nginx).await, LoginError::ClientCertificate(None)));
    }

    #[tokio::test]
    async fn refresh_and_whoami_classify_statuses_the_same_way() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/auth/refresh")).respond_with(ResponseTemplate::new(403)).mount(&server).await;
        Mock::given(method("GET")).and(path("/api/me")).respond_with(ResponseTemplate::new(502)).mount(&server).await;
        let client = Client::new(&server.uri());
        assert!(matches!(client.refresh("r").await.unwrap_err(), LoginError::Blocked(403)));
        assert!(matches!(client.whoami().await.unwrap_err(), LoginError::HttpStatus(502)));
    }

    #[tokio::test]
    async fn whoami_names_the_user_a_token_belongs_to() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .and(header("authorization", "Bearer an-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "user-9", "username": "jane", "type": "user", "mediaProgress": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/api/me")).respond_with(ResponseTemplate::new(401)).mount(&server).await;

        let accepted = Client::with_bearer_token(&server.uri(), "an-api-key").unwrap();
        assert_eq!(accepted.whoami().await.unwrap(), Whoami { user_id: "user-9".into(), username: "jane".into() });
        let rejected = Client::with_bearer_token(&server.uri(), "revoked").unwrap();
        assert!(matches!(rejected.whoami().await.unwrap_err(), LoginError::InvalidCredentials));
    }

    #[tokio::test]
    async fn whoami_on_something_that_is_not_a_user_is_an_unexpected_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/me")).respond_with(ResponseTemplate::new(200).set_body_string("<html>hello</html>")).mount(&server).await;
        let err = Client::new(&server.uri()).whoami().await.unwrap_err();
        assert!(matches!(err, LoginError::UnexpectedResponse(_)), "{err:?}");
    }

    /// The peer's alerts about the client certificate are told apart from trouble with the
    /// server's own certificate, which uses different words.
    #[test]
    fn client_certificate_alerts_are_not_server_certificate_failures() {
        for text in [
            "error sending request → SSL error: tlsv13 alert certificate required",
            "tlsv1 alert unknown ca",
            "sslv3 alert bad certificate",
            "TLSV1 ALERT ACCESS DENIED",
        ] {
            assert!(is_client_certificate_text(text), "{text}");
        }
        for text in [
            "certificate verify failed: self-signed certificate",
            "certificate verify failed: certificate has expired",
            "certificate verify failed: unable to get local issuer certificate",
            "hostname mismatch",
            "connection reset by peer",
        ] {
            assert!(!is_client_certificate_text(text), "{text}");
        }
    }

    #[tokio::test]
    async fn connection_refused_surfaces_as_connect_error_with_details() {
        // Nothing listens on this address, so this fails immediately without any real network —
        // fast and hermetic, unlike the TLS test below.
        let client = Client::new("http://127.0.0.1:1");
        let err = client.login("jane", "hunter2").await.unwrap_err();
        assert!(matches!(err, LoginError::Connect(_)), "got {err:?}");
        assert!(err.details().is_some(), "a Connect error should carry raw details");
    }

    /// Confirms `classify_transport_error`'s TLS detection against a real handshake failure, not
    /// just the marker-string logic in isolation — badssl.com's self-signed subdomain exists
    /// specifically for tests like this one. `#[ignore]`d so `cargo test --workspace` stays
    /// hermetic; run explicitly with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn untrusted_certificate_surfaces_as_tls_error_with_details() {
        let client = Client::new("https://self-signed.badssl.com");
        let err = client.login("jane", "hunter2").await.unwrap_err();
        assert!(matches!(err, LoginError::Tls(_)), "got {err:?}");
        let details = err.details().expect("a Tls error should carry raw details");
        assert!(
            details.to_lowercase().contains("cert") || details.to_lowercase().contains("ssl"),
            "expected certificate-related detail text, got: {details}"
        );
    }

    /// End-to-end against the real public demo server: logs in, finds a real library and item,
    /// then confirms `get_item_playback_info` actually returns a usable audio file — this is what
    /// caught, live, that the real server needs the login the same way every other call does
    /// (`with_bearer_token`, not a plain `Client::new`). `#[ignore]`d; run with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn get_item_playback_info_against_the_live_demo_server() {
        const DEMO_SERVER_URL: &str = "https://audiobooks.dev/audiobookshelf";
        let login = Client::new(DEMO_SERVER_URL).login("demo", "demo").await.unwrap();
        let api = Client::with_bearer_token(DEMO_SERVER_URL, &login.access_token).unwrap();

        let libraries = api.get_libraries().await.unwrap().into_inner().libraries;
        let library_id = libraries[0].id.clone().expect("the demo server's first library should have an id");
        let items = api.get_library_items_with_media(&library_id.to_string()).await.unwrap();
        let item = items.first().expect("the demo server's first library should have at least one item");

        let info = api.get_item_playback_info(&item.id).await.unwrap();
        let audio_file = info.audio_files.first().expect("a real item should have at least one audio file");
        assert!(!audio_file.ino.is_empty());
        assert!(audio_file.duration_seconds > 0.0);
        if let Some(chapter) = info.chapters.first() {
            assert!(!chapter.title.is_empty());
            assert!(chapter.end_seconds > chapter.start_seconds);
        }
    }

    /// End-to-end against the real public demo server: logs in, updates progress for a real item,
    /// then confirms via `GET /api/me` that it actually landed server-side — `PATCH
    /// /api/me/progress/:id` returning `200 OK` isn't itself proof the server persisted anything.
    /// `#[ignore]`d; run with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn update_media_progress_against_the_live_demo_server() {
        const DEMO_SERVER_URL: &str = "https://audiobooks.dev/audiobookshelf";
        let login = Client::new(DEMO_SERVER_URL).login("demo", "demo").await.unwrap();
        let api = Client::with_bearer_token(DEMO_SERVER_URL, &login.access_token).unwrap();

        let libraries = api.get_libraries().await.unwrap().into_inner().libraries;
        let library_id = libraries[0].id.clone().expect("the demo server's first library should have an id");
        let items = api.get_library_items_with_media(&library_id.to_string()).await.unwrap();
        let item = items.first().expect("the demo server's first library should have at least one item");

        api.update_media_progress(&item.id, 77.0, item.duration_seconds, false).await.unwrap();

        let me: serde_json::Value =
            api.client().get(format!("{DEMO_SERVER_URL}/api/me")).send().await.unwrap().json().await.unwrap();
        let synced = me["mediaProgress"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["libraryItemId"] == item.id)
            .expect("the item just updated should appear in mediaProgress");
        assert_eq!(synced["currentTime"].as_f64().unwrap(), 77.0);
    }

    /// End-to-end against the real public demo server: pushes a known progress value, then reads
    /// it back through both `get_media_progress` (the single-item endpoint) and
    /// `get_all_media_progress` (the bulk `/api/me` endpoint) and checks both agree with what was
    /// written. `#[ignore]`d; run with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn get_media_progress_against_the_live_demo_server() {
        const DEMO_SERVER_URL: &str = "https://audiobooks.dev/audiobookshelf";
        let login = Client::new(DEMO_SERVER_URL).login("demo", "demo").await.unwrap();
        let api = Client::with_bearer_token(DEMO_SERVER_URL, &login.access_token).unwrap();

        let libraries = api.get_libraries().await.unwrap().into_inner().libraries;
        let library_id = libraries[0].id.clone().expect("the demo server's first library should have an id");
        let items = api.get_library_items_with_media(&library_id.to_string()).await.unwrap();
        let item = items.first().expect("the demo server's first library should have at least one item");

        api.update_media_progress(&item.id, 99.0, item.duration_seconds, false).await.unwrap();

        let single = api.get_media_progress(&item.id).await.unwrap().expect("progress was just written");
        assert_eq!(single.current_time_seconds, 99.0);

        let all = api.get_all_media_progress().await.unwrap();
        let same_item = all.iter().find(|p| p.library_item_id == item.id).expect("should also appear in the bulk list");
        assert_eq!(same_item.current_time_seconds, 99.0);
    }

    async fn list_of(results: serde_json::Value) -> Vec<LibraryItemSummary> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": results })))
            .mount(&server)
            .await;
        Client::new(&server.uri()).get_library_items_with_media("lib-1").await.unwrap()
    }

    /// Everything the list returns beyond the basics is kept: Book Details facts, the total size,
    /// the chapter count, tags, the cover flag and the server's sort keys.
    #[tokio::test]
    async fn the_library_list_keeps_the_extra_fields() {
        let items = list_of(serde_json::json!([{
            "id": "item-1",
            "media": {
                "duration": 3600.0,
                "size": 340_000_000u64,
                "numChapters": 19,
                "tags": ["classic", "  ", "fantasy"],
                "coverPath": "/audiobooks/The Hobbit/cover.jpg",
                "metadata": {
                    "title": "The Hobbit", "titleIgnorePrefix": "Hobbit, The",
                    "authorName": "J.R.R. Tolkien", "authorNameLF": "Tolkien, J.R.R.",
                    "subtitle": "There and Back Again", "publishedYear": "1937", "publisher": "Allen & Unwin",
                    "language": "English", "explicit": false, "isbn": "9780261102217", "asin": ""
                }
            }
        }]))
        .await;
        let item = &items[0];
        assert_eq!(item.subtitle.as_deref(), Some("There and Back Again"));
        assert_eq!((item.size_bytes, item.num_chapters), (Some(340_000_000), Some(19)));
        assert_eq!(item.tags, ["classic", "fantasy"], "blank tags are dropped");
        assert_eq!((item.published_year.as_deref(), item.publisher.as_deref(), item.language.as_deref()), (Some("1937"), Some("Allen & Unwin"), Some("English")));
        assert_eq!((item.explicit, item.isbn.as_deref(), item.asin.as_deref()), (Some(false), Some("9780261102217"), None), "a blank ASIN is none");
        assert_eq!(item.has_cover, Some(true));
        assert_eq!((item.title_sort.as_deref(), item.author_sort.as_deref()), (Some("Hobbit, The"), Some("Tolkien, J.R.R.")));
    }

    /// One book with a field of the wrong type must not cost the library its other books, or even
    /// its own other fields.
    #[tokio::test]
    async fn odd_typed_extra_fields_are_ignored_not_fatal() {
        let items = list_of(serde_json::json!([
            {
                "id": "odd",
                "media": {
                    "size": "huge", "numChapters": {"n": 1}, "tags": "not-a-list", "coverPath": 12,
                    "metadata": { "title": "Odd", "subtitle": 5, "publishedYear": 1999, "publisher": ["x"], "explicit": "yes", "isbn": null,
                                  "titleIgnorePrefix": [], "authorNameLF": {} }
                }
            },
            { "id": "fine", "media": { "metadata": { "title": "Fine" } } }
        ]))
        .await;
        assert_eq!(items.len(), 2, "the other book is still listed");
        let odd = &items[0];
        assert_eq!((odd.title.as_str(), odd.size_bytes, odd.num_chapters, odd.has_cover), ("Odd", None, None, None));
        assert!(odd.tags.is_empty());
        assert_eq!(odd.published_year.as_deref(), Some("1999"), "a year sent as a number is still a year");
        assert_eq!((odd.subtitle.as_deref(), odd.publisher.as_deref(), odd.explicit, odd.isbn.as_deref(), odd.title_sort.as_deref(), odd.author_sort.as_deref()), (None, None, None, None, None, None));
    }

    /// An older or trimmed response: nothing extra, and nothing claimed. In particular a missing
    /// `coverPath` is "unknown", never "no cover".
    #[tokio::test]
    async fn missing_extra_fields_are_unknown() {
        let items = list_of(serde_json::json!([{ "id": "item-1", "media": { "metadata": { "title": "Bare" } } }])).await;
        let item = &items[0];
        assert_eq!((item.subtitle.clone(), item.size_bytes, item.num_chapters, item.has_cover, item.title_sort.clone(), item.author_sort.clone()), (None, None, None, None, None, None));
        assert!(item.tags.is_empty());
        let none = list_of(serde_json::json!([{ "id": "a", "media": { "coverPath": null, "metadata": { "title": "No cover" } } }, { "id": "b", "media": { "coverPath": "  ", "metadata": { "title": "Blank cover" } } }])).await;
        assert_eq!((none[0].has_cover, none[1].has_cover), (Some(false), Some(false)), "null or blank is the server saying there is no cover");
    }

    #[tokio::test]
    async fn get_library_items_with_media_parses_title_author_and_duration() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "item-1",
                    "addedAt": 1_700_000_000_000i64,
                    "media": {
                        "duration": 3600.5,
                        "metadata": {
                            "title": "Project Hail Mary",
                            "authorName": "Andy Weir",
                            "narratorName": "Ray Porter",
                            "description": "A lone astronaut...",
                            "seriesName": "Standalone Series",
                            "genres": ["Sci-Fi", "Thriller"],
                        }
                    }
                }]
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let items = client.get_library_items_with_media("lib-1").await.unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "item-1");
        assert_eq!(items[0].title, "Project Hail Mary");
        assert_eq!(items[0].author.as_deref(), Some("Andy Weir"));
        assert_eq!(items[0].narrator.as_deref(), Some("Ray Porter"));
        assert_eq!(items[0].duration_seconds, 3600.5);
        assert_eq!(items[0].added_at_ms, 1_700_000_000_000);
        assert_eq!(items[0].series_name.as_deref(), Some("Standalone Series"));
        assert_eq!(items[0].genres, vec!["Sci-Fi".to_string(), "Thriller".to_string()]);
    }

    #[tokio::test]
    async fn get_library_items_with_media_defaults_series_and_genres_when_absent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "item-1",
                    "media": { "metadata": { "title": "No Series Or Genres" } }
                }]
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let items = client.get_library_items_with_media("lib-1").await.unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].series_name, None);
        assert_eq!(items[0].genres, Vec::<String>::new());
    }

    #[tokio::test]
    async fn get_library_items_with_media_reads_blank_author_narrator_and_series_as_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "item-1",
                    "media": { "metadata": { "title": "Anonymous", "authorName": "", "narratorName": " ", "seriesName": "" } }
                }]
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let items = client.get_library_items_with_media("lib-1").await.unwrap();

        assert_eq!((items[0].author.as_deref(), items[0].narrator.as_deref(), items[0].series_name.as_deref()), (None, None, None));
    }

    #[tokio::test]
    async fn get_library_items_with_media_skips_entries_missing_media_or_title() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    { "id": "no-media" },
                    { "id": "no-title", "media": { "metadata": {} } },
                    {
                        "id": "item-1",
                        "media": { "metadata": { "title": "Valid Item" } }
                    },
                ]
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let items = client.get_library_items_with_media("lib-1").await.unwrap();

        assert_eq!(items.len(), 1, "only the well-formed item should survive");
        assert_eq!(items[0].id, "item-1");
        assert_eq!(items[0].duration_seconds, 0.0, "missing duration defaults to 0.0");
    }

    #[tokio::test]
    async fn get_library_items_with_media_propagates_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/items"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let err = client.get_library_items_with_media("lib-1").await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::UnexpectedResponse(_)));
    }

    #[tokio::test]
    async fn get_library_series_summaries_parses_series_and_book_sequences() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/series"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "series-1",
                    "name": "Foundation",
                    "books": [
                        { "id": "item-1", "sequence": "1" },
                        { "id": "item-2", "sequence": null }
                    ]
                }]
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let series = client.get_library_series_summaries("lib-1").await.unwrap();

        assert_eq!(series.len(), 1);
        assert_eq!(series[0].id, "series-1");
        assert_eq!(series[0].name, "Foundation");
        assert_eq!(series[0].books.len(), 2);
        assert_eq!(series[0].books[0].item_id, "item-1");
        assert_eq!(series[0].books[0].sequence.as_deref(), Some("1"));
        assert_eq!(series[0].books[1].item_id, "item-2");
        assert_eq!(series[0].books[1].sequence, None, "a null sequence is a real, if less common, server-side possibility");
    }

    #[tokio::test]
    async fn get_library_series_summaries_skips_a_series_or_book_missing_its_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/series"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    { "name": "No Id Series", "books": [{ "id": "item-1", "sequence": "1" }] },
                    {
                        "id": "series-1",
                        "name": "Foundation",
                        "books": [{ "sequence": "1" }, { "id": "item-2", "sequence": "2" }]
                    }
                ]
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let series = client.get_library_series_summaries("lib-1").await.unwrap();

        assert_eq!(series.len(), 1, "the id-less series must be skipped entirely, not just its malformed book");
        assert_eq!(series[0].id, "series-1");
        assert_eq!(series[0].books.len(), 1, "the id-less book within a valid series is skipped without failing the series");
        assert_eq!(series[0].books[0].item_id, "item-2");
    }

    #[tokio::test]
    async fn get_library_series_summaries_propagates_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/libraries/lib-1/series"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let err = client.get_library_series_summaries("lib-1").await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::UnexpectedResponse(_)));
    }

    #[tokio::test]
    async fn get_item_playback_info_parses_audio_files() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [
                        { "ino": "111", "duration": 1800.5 },
                        { "ino": "222", "duration": 1200.0 },
                    ]
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let info = client.get_item_playback_info("item-1").await.unwrap();

        assert_eq!(info.audio_files.len(), 2);
        assert_eq!(info.audio_files[0].ino, "111");
        assert_eq!(info.audio_files[0].duration_seconds, 1800.5);
        assert_eq!(info.audio_files[1].ino, "222");
    }

    #[tokio::test]
    async fn get_item_playback_info_with_no_audio_files_is_an_empty_vec_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [] }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let info = client.get_item_playback_info("item-1").await.unwrap();
        assert!(info.audio_files.is_empty());
    }

    #[tokio::test]
    async fn get_item_playback_info_skips_audio_files_missing_ino() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [
                        { "duration": 1800.5 },
                        { "ino": "222", "duration": 1200.0 },
                    ]
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let info = client.get_item_playback_info("item-1").await.unwrap();

        assert_eq!(info.audio_files.len(), 1);
        assert_eq!(info.audio_files[0].ino, "222");
    }

    #[tokio::test]
    async fn get_item_playback_info_propagates_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let err = client.get_item_playback_info("item-1").await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::UnexpectedResponse(_)));
    }

    #[tokio::test]
    async fn get_item_playback_info_parses_chapters() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [{ "ino": "1", "duration": 3600.0 }],
                    "chapters": [
                        { "id": 0, "start": 0.0, "end": 100.0, "title": "Introduction" },
                        { "id": 1, "start": 100.0, "end": 3600.0, "title": "Chapter 1" },
                    ]
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let info = client.get_item_playback_info("item-1").await.unwrap();

        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].title, "Introduction");
        assert_eq!(info.chapters[0].start_seconds, 0.0);
        assert_eq!(info.chapters[0].end_seconds, 100.0);
        assert_eq!(info.chapters[1].title, "Chapter 1");
    }

    #[tokio::test]
    async fn get_item_playback_info_parses_audio_file_sizes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [
                        { "ino": "1", "duration": 1800.0, "metadata": { "size": 33_000_000 } },
                        { "ino": "2", "duration": 1800.0 },
                        { "ino": "3", "duration": 1800.0, "metadata": { "size": null } },
                    ],
                    "chapters": []
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let info = client.get_item_playback_info("item-1").await.unwrap();

        assert_eq!(info.audio_files.len(), 3);
        assert_eq!(info.audio_files[0].size_bytes, Some(33_000_000));
        assert_eq!(info.audio_files[1].size_bytes, None, "no metadata object at all is unknown, not zero");
        assert_eq!(info.audio_files[2].size_bytes, None, "an explicit null size is unknown, not zero");
    }

    #[tokio::test]
    async fn get_item_playback_info_skips_malformed_chapters() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": {
                    "audioFiles": [{ "ino": "1", "duration": 3600.0 }],
                    "chapters": [
                        { "id": 0, "start": 0.0, "title": "Missing end" },
                        { "id": 1, "start": 10.0, "end": 20.0, "title": "Valid" },
                    ]
                }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let info = client.get_item_playback_info("item-1").await.unwrap();

        assert_eq!(info.chapters.len(), 1, "a chapter missing a required field should be skipped, not fail the call");
        assert_eq!(info.chapters[0].title, "Valid");
    }

    #[tokio::test]
    async fn get_item_playback_info_with_no_chapters_is_an_empty_vec_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media": { "audioFiles": [{ "ino": "1", "duration": 3600.0 }] }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let info = client.get_item_playback_info("item-1").await.unwrap();
        assert!(info.chapters.is_empty());
    }

    #[tokio::test]
    async fn get_item_cover_returns_bytes_and_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/cover"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "image/webp").set_body_bytes(vec![1, 2, 3, 4]))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let cover = client.get_item_cover("item-1").await.unwrap();
        assert_eq!(cover.bytes, vec![1, 2, 3, 4]);
        assert_eq!(cover.content_type, "image/webp");
    }

    #[tokio::test]
    async fn get_item_cover_defaults_to_jpeg_when_no_content_type_is_sent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/cover"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let cover = client.get_item_cover("item-1").await.unwrap();
        assert_eq!(cover.content_type, "image/jpeg");
    }

    #[tokio::test]
    async fn get_item_cover_propagates_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/items/item-1/cover")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

        let client = Client::new(&server.uri());
        let err = client.get_item_cover("item-1").await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::UnexpectedResponse(_)));
    }

    #[tokio::test]
    async fn get_item_cover_original_asks_for_the_uploaded_file_and_reports_progress() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/cover"))
            .and(query_param("raw", "1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Type", "image/png").set_body_bytes(vec![7; 5000]))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let seen = std::cell::RefCell::new(Vec::new());
        let cover = client.get_item_cover_original("item-1", |received, total| seen.borrow_mut().push((received, total))).await.unwrap();
        assert_eq!(cover.bytes.len(), 5000);
        assert_eq!(cover.content_type, "image/png");
        let seen = seen.into_inner();
        assert_eq!(seen.first(), Some(&(0, Some(5000))));
        assert_eq!(seen.last(), Some(&(5000, Some(5000))));
    }

    #[tokio::test]
    async fn get_item_cover_tells_a_missing_cover_apart() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/items/item-1/cover")).respond_with(ResponseTemplate::new(404)).mount(&server).await;

        let client = Client::new(&server.uri());
        let err = client.get_item_cover("item-1").await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn get_item_file_response_without_range_returns_the_full_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "4").insert_header("Content-Type", "audio/mpeg").set_body_bytes(vec![1, 2, 3, 4]))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let file = client.get_item_file_response("item-1", "ino-1", None).await.unwrap();
        assert!(!file.resumed, "a plain 200 response was not a resumed range");
        assert_eq!(file.total_size, Some(4));
        assert_eq!(file.content_type.as_deref(), Some("audio/mpeg"));
        let bytes = file.response.bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), &[1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn get_item_file_response_sends_a_range_header_and_recognizes_206() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .and(header("Range", "bytes=100-"))
            .respond_with(ResponseTemplate::new(206).insert_header("Content-Range", "bytes 100-103/104").set_body_bytes(vec![9, 9, 9, 9]))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let file = client.get_item_file_response("item-1", "ino-1", Some(100)).await.unwrap();
        assert!(file.resumed, "a 206 response means the server actually honored the Range request");
        assert_eq!(file.total_size, Some(104), "total_size must be the whole file's size, not just this response's 4-byte body");
    }

    #[tokio::test]
    async fn get_item_file_response_detects_a_server_that_ignores_range() {
        let server = MockServer::start().await;
        // Some servers reply 200 with the *full* body even when a Range header was sent — the
        // caller must be able to tell this apart from a real 206 resume, or it would corrupt a
        // partially-downloaded file by blindly appending onto it.
        Mock::given(method("GET"))
            .and(path("/api/items/item-1/file/ino-1"))
            .and(header("Range", "bytes=100-"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 104]))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let file = client.get_item_file_response("item-1", "ino-1", Some(100)).await.unwrap();
        assert!(!file.resumed, "a 200 in response to a Range request must not be treated as resumed");
    }

    #[tokio::test]
    async fn get_item_file_response_surfaces_the_status_on_a_client_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/items/item-1/file/ino-1")).respond_with(ResponseTemplate::new(404)).mount(&server).await;

        let client = Client::new(&server.uri());
        let err = client.get_item_file_response("item-1", "ino-1", None).await.unwrap_err();
        assert!(!err.is_retryable(), "a 404 should never be retried");
        assert!(matches!(err, TrackFileError::Status(status) if status == reqwest::StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn get_item_file_response_a_server_error_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/items/item-1/file/ino-1")).respond_with(ResponseTemplate::new(503)).mount(&server).await;

        let client = Client::new(&server.uri());
        let err = client.get_item_file_response("item-1", "ino-1", None).await.unwrap_err();
        assert!(err.is_retryable(), "a 5xx should be retried");
    }

    #[tokio::test]
    async fn update_media_progress_sends_the_expected_body() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/me/progress/item-1"))
            .and(body_partial_json(serde_json::json!({
                "currentTime": 42.5,
                "duration": 200.0,
                "isFinished": false,
            })))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        client.update_media_progress("item-1", 42.5, 200.0, false).await.unwrap();
    }

    #[tokio::test]
    async fn update_media_progress_propagates_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("PATCH"))
            .and(path("/api/me/progress/item-2"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let err = client.update_media_progress("item-1", 42.5, 200.0, false).await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::NotFound(_)), "a 404 means the item is gone from the server: {err:?}");
        let err = client.update_media_progress("item-2", 42.5, 200.0, false).await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::UnexpectedResponse(_)));
    }

    #[tokio::test]
    async fn get_media_progress_parses_an_existing_record() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "libraryItemId": "item-1",
                "currentTime": 123.5,
                "duration": 3600.0,
                "isFinished": false,
                "lastUpdate": 1_700_000_000_000i64,
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let progress = client.get_media_progress("item-1").await.unwrap().expect("a progress record exists");
        assert_eq!(progress.library_item_id, "item-1");
        assert_eq!(progress.current_time_seconds, 123.5);
        assert_eq!(progress.last_update_ms, 1_700_000_000_000);
    }

    #[tokio::test]
    async fn get_media_progress_with_no_record_is_none_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        assert!(client.get_media_progress("item-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_media_progress_propagates_other_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me/progress/item-1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let err = client.get_media_progress("item-1").await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::UnexpectedResponse(_)));
    }

    fn device_info() -> DeviceInfo {
        DeviceInfo {
            device_id: "dev-1".to_string(),
            client_name: "Audiobooklet".to_string(),
            client_version: "1.0".to_string(),
            manufacturer: Some("Purism".to_string()),
            model: None,
        }
    }

    fn local_session(id: &str) -> LocalSession {
        LocalSession {
            id: id.to_string(),
            library_item_id: "item-1".to_string(),
            library_id: Some("lib-1".to_string()),
            media_type: "book",
            play_method: 3,
            media_player: "gstreamer",
            display_title: "Book".to_string(),
            display_author: None,
            duration: 3600.0,
            start_time: 100.0,
            current_time: 160.0,
            time_listening: 60.0,
            started_at: 1_000,
            updated_at: 61_000,
        }
    }

    #[tokio::test]
    async fn sync_local_sessions_sends_one_batch_and_reads_each_result_and_the_server_clock() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/session/local-all"))
            .and(body_partial_json(serde_json::json!({
                "deviceInfo": { "deviceId": "dev-1", "clientName": "Audiobooklet", "clientVersion": "1.0", "manufacturer": "Purism" },
                "sessions": [
                    { "id": "s-1", "libraryItemId": "item-1", "libraryId": "lib-1", "mediaType": "book", "playMethod": 3,
                      "mediaPlayer": "gstreamer", "displayTitle": "Book", "duration": 3600.0, "startTime": 100.0,
                      "currentTime": 160.0, "timeListening": 60.0, "startedAt": 1000, "updatedAt": 61000 },
                    { "id": "s-2" },
                ]
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("date", "Wed, 14 Oct 2026 17:39:17 GMT")
                    .set_body_json(serde_json::json!({ "results": [
                        { "id": "s-1", "success": true, "progressSynced": true },
                        { "id": "s-2", "success": false, "error": "Media item not found" },
                    ]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let sync = Client::new(&server.uri()).sync_local_sessions(&device_info(), &[local_session("s-1"), local_session("s-2")]).await.unwrap();
        assert_eq!(sync.results.len(), 2);
        assert!(sync.results[0].success && sync.results[0].progress_synced);
        assert!(sync.results[1].is_item_missing());
        assert!(!sync.results[0].is_item_missing());
        assert_eq!(sync.server_date.unwrap().to_rfc3339(), "2026-10-14T17:39:17+00:00");
    }

    #[tokio::test]
    async fn sync_local_sessions_leaves_unknown_fields_out_of_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/session/local-all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [] })))
            .mount(&server)
            .await;
        let mut session = local_session("s-1");
        session.library_id = None;
        Client::new(&server.uri()).sync_local_sessions(&device_info(), &[session]).await.unwrap();

        let body: serde_json::Value = server.received_requests().await.unwrap()[0].body_json().unwrap();
        assert!(body["deviceInfo"].get("model").is_none(), "no model known: not sent at all");
        assert!(body["sessions"][0].get("libraryId").is_none());
        assert!(body["sessions"][0].get("displayAuthor").is_none());
        assert!(body["sessions"][0].get("chapters").is_none(), "chapters are the server's to fill in");
    }

    #[tokio::test]
    async fn sync_local_sessions_maps_a_missing_endpoint_and_rejected_credentials() {
        for (status, expect_not_found) in [(404, true), (405, true), (401, false), (500, false)] {
            let server = MockServer::start().await;
            Mock::given(method("POST")).and(path("/api/session/local-all")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
            let err = Client::new(&server.uri()).sync_local_sessions(&device_info(), &[local_session("s-1")]).await.unwrap_err();
            match status {
                401 => assert!(matches!(err, LibraryItemsError::Unauthorized(401))),
                500 => assert!(matches!(err, LibraryItemsError::UnexpectedResponse(_))),
                _ => assert_eq!(matches!(err, LibraryItemsError::NotFound(_)), expect_not_found, "HTTP {status}"),
            }
        }
    }

    #[test]
    fn server_date_reads_the_http_date_header_and_ignores_garbage() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert!(server_date(&headers).is_none());
        headers.insert(reqwest::header::DATE, "not a date".parse().unwrap());
        assert!(server_date(&headers).is_none());
        headers.insert(reqwest::header::DATE, "Thu, 15 Oct 2026 06:56:02 GMT".parse().unwrap());
        assert_eq!(server_date(&headers).unwrap().timestamp(), 1_792_047_362);
    }

    #[tokio::test]
    async fn get_all_media_progress_parses_every_record() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "mediaProgress": [
                    { "libraryItemId": "item-1", "currentTime": 10.0, "duration": 100.0, "isFinished": false, "lastUpdate": 1 },
                    { "libraryItemId": "item-2", "currentTime": 20.0, "duration": 200.0, "isFinished": true, "lastUpdate": 2 },
                ]
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let all = client.get_all_media_progress().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].library_item_id, "item-1");
        assert_eq!(all[1].library_item_id, "item-2");
        assert!(all[1].is_finished);
    }

    #[tokio::test]
    async fn get_all_media_progress_with_no_progress_is_an_empty_vec() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "mediaProgress": [] })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        assert!(client.get_all_media_progress().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_all_media_progress_propagates_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/api/me")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

        let client = Client::new(&server.uri());
        let err = client.get_all_media_progress().await.unwrap_err();
        assert!(matches!(err, LibraryItemsError::UnexpectedResponse(_)));
    }

    #[tokio::test]
    async fn missing_access_token_in_response_is_an_unexpected_response_error() {
        // Simulates hitting a server that ignored x-return-tokens (e.g. an older version) —
        // this must not panic or silently produce an empty token.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": { "id": "user-1", "username": "jane" }
            })))
            .mount(&server)
            .await;

        let client = Client::new(&server.uri());
        let err = client.login("jane", "hunter2").await.unwrap_err();
        assert!(matches!(err, LoginError::UnexpectedResponse(_)));
    }

    /// A one-connection HTTP server for the refresh tests: accepts, reads the request up to the
    /// blank line, then hands the stream to `respond` (which may write, stall or just drop it).
    fn one_shot_server(respond: impl FnOnce(std::net::TcpStream) + Send + 'static) -> String {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the test client should connect");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                request.push(byte[0]);
            }
            respond(stream);
        });
        format!("http://{addr}")
    }

    /// The failure that cost a real login: the server answered 200 (and so had already swapped
    /// the pair), then the ~30 KB reply trickled in slower than the client's whole-request limit
    /// and was thrown away. The reply now gets its own, longer wait — here longer than both the
    /// client's own limit and the wait for the answer.
    #[tokio::test]
    async fn refresh_waits_for_a_slow_reply_once_the_server_has_answered() {
        use std::io::Write;
        let url = one_shot_server(|mut stream| {
            let body = serde_json::json!({
                "user": { "id": "user-1", "username": "jane", "accessToken": "new-access", "refreshToken": "new-refresh" }
            })
            .to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            stream.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(600));
            stream.write_all(body.as_bytes()).unwrap();
        });
        let client = Client::with_options_and_headers(&url, &ConnectionOptions::default(), None, std::time::Duration::from_millis(200)).unwrap();

        let result = client
            .refresh_within("old-refresh", std::time::Duration::from_millis(300), std::time::Duration::from_secs(5))
            .await
            .expect("the reply arrived, just slowly");

        assert_eq!(result.access_token, "new-access");
        assert_eq!(result.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[tokio::test]
    async fn a_refresh_the_server_never_answers_may_have_been_acted_on() {
        let url = one_shot_server(|_stream| std::thread::sleep(std::time::Duration::from_secs(60)));
        let client = Client::new(&url);

        let started = std::time::Instant::now();
        let err = client
            .refresh_within("old-refresh", std::time::Duration::from_millis(200), std::time::Duration::from_secs(5))
            .await
            .unwrap_err();

        assert!(matches!(err, LoginError::NoAnswer), "got {err:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "the wait for the answer is what ran out");
        assert!(err.server_may_have_acted(), "the request went out; the server may have swapped the pair");
    }

    #[tokio::test]
    async fn a_refresh_cut_off_after_it_was_sent_may_have_been_acted_on() {
        let url = one_shot_server(drop);
        let err = Client::new(&url).refresh("old-refresh").await.unwrap_err();

        assert!(err.server_may_have_acted(), "the connection closed after the request was sent: {err:?}");
    }

    #[tokio::test]
    async fn a_refresh_that_never_connected_was_not_acted_on() {
        let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let err = Client::new(&format!("http://{addr}")).refresh("old-refresh").await.unwrap_err();

        assert!(matches!(err, LoginError::Connect(_)), "got {err:?}");
        assert!(!err.server_may_have_acted());
    }

    #[tokio::test]
    async fn a_rejected_refresh_was_not_acted_on() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/auth/refresh")).respond_with(ResponseTemplate::new(401)).mount(&server).await;

        let err = Client::new(&server.uri()).refresh("old-refresh").await.unwrap_err();

        assert!(matches!(err, LoginError::SessionExpired));
        assert!(!err.server_may_have_acted());
    }

    /// The kernel version in the User-Agent: only the leading number survives, and anything
    /// that isn't one leaves the old `(Linux)`.
    #[test]
    fn kernel_version_keeps_only_the_leading_number() {
        assert_eq!(kernel_version("6.6.101-librem5\n").as_deref(), Some("6.6.101"));
        assert_eq!(kernel_version("6.12.0").as_deref(), Some("6.12.0"));
        assert_eq!(kernel_version("6").as_deref(), Some("6"));
        assert_eq!(kernel_version("5.15.0-122-generic").as_deref(), Some("5.15.0"));
        assert_eq!(kernel_version("6.1.0+rpt-rpi-v8").as_deref(), Some("6.1.0"));
        assert_eq!(kernel_version("6.8.").as_deref(), Some("6.8"));
        assert_eq!(kernel_version(&"1".repeat(100)).map(|v| v.len()), Some(32));
        for odd in ["", "\n", "abc", "-1", ".5", "v6.6", "  ", "６.６"] {
            assert_eq!(kernel_version(odd), None, "{odd:?}");
        }
        assert_eq!(user_agent_for(Some("6.6.101")), concat!("Audiobooklet/", env!("CARGO_PKG_VERSION"), " (Linux 6.6.101)"));
        assert_eq!(user_agent_for(None), concat!("Audiobooklet/", env!("CARGO_PKG_VERSION"), " (Linux)"));
    }

    /// The app identifies itself as `Audiobooklet/<version> (Linux <kernel>)` unless a server's
    /// settings say otherwise — never without a User-Agent (a `"-"` in the server's log), and
    /// never with the Audiobookshelf name.
    #[tokio::test]
    async fn requests_send_the_app_s_own_user_agent_unless_overridden() {
        let agent = default_user_agent();
        assert!(agent.starts_with(concat!("Audiobooklet/", env!("CARGO_PKG_VERSION"), " (Linux")), "{agent}");
        assert!(!agent.to_lowercase().contains("audiobookshelf"));
        let os = agent.split_once(" (").unwrap().1;
        assert!(os == "Linux)" || os.strip_prefix("Linux ").and_then(|rest| rest.strip_suffix(')')).is_some_and(|kernel| kernel_version(kernel).as_deref() == Some(kernel)), "{agent}");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login"))
            .and(header("user-agent", default_user_agent()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user": { "id": "user-1", "username": "jane", "accessToken": "abc123" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/libraries"))
            .and(header("user-agent", "MyAgent/1.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "libraries": [] })))
            .mount(&server)
            .await;

        Client::with_default_options(&server.uri()).login("jane", "hunter2").await.expect("sent the default agent");

        let options = ConnectionOptions { user_agent: Some("MyAgent/1.0".to_string()), ..Default::default() };
        let client = Client::with_bearer_token_and_options(&server.uri(), "abc123", std::time::Duration::from_secs(5), &options).unwrap();
        client.get_libraries().await.expect("sent the override instead");
    }
}
