//! Resolves a server's row (the `servers` table) into everything a real server connection
//! needs — the base URL to talk to and the transport options to honor — as one
//! [`ConnectionTarget`] that callers thread through every server-facing function instead of a
//! bare `server_url`. This is what makes the Connection page's Advanced settings (custom
//! headers, TLS verification, client certificate, user agent, local network address) apply to
//! actual traffic rather than sitting in the database as decoration.
//!
//! Layering: `abs-api` is the only crate that assembles reqwest primitives (headers, TLS
//! flags, client identity) — [`ConnectionTarget`] carries them as plain data and asks `abs-api`
//! to mint clients. This module owns the *resolution* policy: parsing/validating what the row
//! stores (tolerantly — a hand-edited or corrupt value degrades, it never crashes a mint) and
//! deciding when the local network address is used instead of the public URL. The probe
//! *result* is injected as a plain `Option<bool>` so the policy stays pure and unit-testable;
//! `Session` (in `auth`) owns the actual probing and its TTL cache.

use std::path::PathBuf;
use std::time::Duration;

use abs_storage::models::Server;

/// Everything a server-facing call needs to reach its server: where to connect (after
/// local-address resolution) and how (the transport options). Built once per task from the
/// server's row and passed down explicitly — no function in this crate re-derives it from a
/// captured URL string, so a settings change is picked up by the next sync, download or
/// playback resolve without any rebuild.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionTarget {
    /// The base URL every request and track URL is built on: the server's own URL, or its
    /// local network address when one is configured and the probe found it reachable.
    pub base_url: String,
    /// Transport options (custom headers, TLS behavior, client certificate, user agent) for
    /// this server, ready for `abs-api`'s minting functions.
    pub options: abs_api::ConnectionOptions,
}

impl ConnectionTarget {
    /// Resolves a server's row into a connection target. `local_reachable` is the probe result
    /// for the row's local network address (`None`: not configured or not determinable — the
    /// public URL is used). Infallible: values that don't parse or validate are dropped with a
    /// warning rather than failing the resolution, since the row may have been written by
    /// anything, and one bad field must not take down syncing or playback. (A *configured but
    /// broken* client certificate stays a hard mint-time error — see
    /// [`ConnectionTarget::api_client`] — because silently dropping an mTLS credential would
    /// turn a clear failure into a baffling server-side rejection.)
    pub fn resolve(server: &Server, local_reachable: Option<bool>) -> Self {
        ConnectionTarget {
            base_url: resolved_base_url(server, local_reachable),
            options: abs_api::ConnectionOptions {
                extra_headers: parse_custom_headers(&server.custom_headers_json),
                disable_ssl_verify: server.disable_ssl_verify,
                client_cert_path: server
                    .client_cert_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from),
                client_cert_password: server
                    .client_cert_password
                    .as_deref()
                    .filter(|password| !password.is_empty())
                    .map(str::to_string),
                user_agent: server
                    .user_agent
                    .as_deref()
                    .map(str::trim)
                    .filter(|user_agent| !user_agent.is_empty())
                    .map(str::to_string),
            },
        }
    }

    /// A target with default options and no local-address logic — what every call site did
    /// before connection settings existed. For tests and fixtures.
    pub fn direct(url: &str) -> Self {
        ConnectionTarget { base_url: url.to_string(), options: abs_api::ConnectionOptions::default() }
    }

    /// An authenticated client for this connection — the standard mint for every server-facing
    /// call. 15s timeout: long enough to tolerate a slow mobile connection, short enough that
    /// a call never hangs indefinitely when the server is simply gone.
    pub fn api_client(&self, access_token: &str) -> Result<abs_api::Client, abs_api::ConnectionBuildError> {
        self.api_client_with_timeout(access_token, Duration::from_secs(15))
    }

    /// Same as [`ConnectionTarget::api_client`] with a caller-chosen timeout — best-effort
    /// background work (covers, progress reconciliation) uses much shorter ones so a bad
    /// connection isn't felt as a hang.
    pub fn api_client_with_timeout(
        &self,
        access_token: &str,
        timeout: Duration,
    ) -> Result<abs_api::Client, abs_api::ConnectionBuildError> {
        abs_api::Client::with_bearer_token_and_options(&self.base_url, access_token, timeout, &self.options)
    }

    /// An unauthenticated client honoring this connection's transport options — for the one
    /// call that runs before any token exists but must still reach the server the same way
    /// (token refresh, which carries the refresh token in a header).
    pub fn plain_client(&self) -> Result<abs_api::Client, abs_api::ConnectionBuildError> {
        abs_api::Client::with_options(&self.base_url, &self.options)
    }

    /// The authenticated stream URL for a single audio file. Track URLs are **baked into a
    /// loaded playback pipeline** for as long as that file plays, so callers holding a stream
    /// across a long session rebuild them with a current token via this rather than replaying
    /// one resolved earlier.
    pub fn track_url(&self, item_id: &str, ino: &str, access_token: &str) -> String {
        format!("{}/api/items/{item_id}/file/{ino}?token={access_token}", self.base_url)
    }

    // Plain accessors for the app's playback mapping (`abs-core` can't name `abs-player`
    // types, so the app translates these into `abs_player::ConnectionProperties` itself).

    /// Headers every playback request should carry (see [`ConnectionTarget::options`]).
    pub fn extra_headers(&self) -> &[(String, String)] {
        &self.options.extra_headers
    }

    /// The user agent playback requests should send: the server's override, else the same
    /// default every API call sends ([`abs_api::default_user_agent`]).
    pub fn user_agent(&self) -> &str {
        self.options.user_agent.as_deref().unwrap_or(abs_api::default_user_agent())
    }

    /// Whether playback should skip certificate verification (see
    /// [`ConnectionTarget::options`]).
    pub fn disable_ssl_verify(&self) -> bool {
        self.options.disable_ssl_verify
    }
}

/// Which base URL a server's traffic uses: the local network address when one is configured
/// *and* known reachable, the public URL otherwise (not configured, probe failed, or probe
/// couldn't tell). Trimmed and trailing-slash-normalized for the local address — the same
/// tolerance `accounts::normalize_url` applies to a user-typed public URL, for the same reason.
fn resolved_base_url(server: &Server, local_reachable: Option<bool>) -> String {
    let local = server
        .local_network_address
        .as_deref()
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .map(|address| address.trim_end_matches('/'));
    match (local, local_reachable) {
        (Some(local), Some(true)) => local.to_string(),
        _ => server.url.clone(),
    }
}

/// The Connection page's custom-headers editor format: one `Name: Value` pair per line.
/// A single source of truth for turning that text into the canonical JSON the `servers` row
/// stores — the UI calls this on save (rendering the error), and nothing else ever writes the
/// column, so what `parse_custom_headers` reads is always this format.
pub fn validate_custom_headers_text(text: &str) -> Result<String, CustomHeadersError> {
    let mut headers = std::collections::BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(CustomHeadersError::MissingColon { line: index + 1 });
        };
        let name = name.trim();
        let value = value.trim();
        if !abs_api::is_valid_header_name(name) {
            return Err(CustomHeadersError::InvalidName { line: index + 1, name: name.to_string() });
        }
        if name.eq_ignore_ascii_case("authorization") {
            return Err(CustomHeadersError::AuthorizationReserved);
        }
        if !abs_api::is_valid_header_value(value) {
            return Err(CustomHeadersError::InvalidValue { line: index + 1 });
        }
        headers.insert(name.to_string(), value.to_string());
    }
    Ok(serde_json::to_string(&headers).expect("a BTreeMap of strings always serializes"))
}

/// Why custom-headers text was rejected — `Display` is the user-facing message.
#[derive(Debug, thiserror::Error)]
pub enum CustomHeadersError {
    #[error("line {line}: expected \"Name: Value\"")]
    MissingColon { line: usize },
    #[error("line {line}: {name:?} is not a valid HTTP header name")]
    InvalidName { line: usize, name: String },
    #[error("line {line}: that value is not a valid HTTP header value")]
    InvalidValue { line: usize },
    #[error("\"authorization\" can't be overridden — it would replace the app's own login token")]
    AuthorizationReserved,
}

/// Parses a `servers` row's `custom_headers_json` into the headers a mint should attach.
/// Tolerant by design: entries that aren't a JSON object of string→string, aren't valid HTTP
/// header names/values, or try to override `authorization` are skipped with a warning — a
/// hand-edited or corrupt row degrades to "fewer custom headers", never to a failed mint.
/// Sorted, so the same row always resolves to the same options (and the same client behavior).
pub fn parse_custom_headers(json: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        // Never log the raw JSON: header values are often reverse-proxy credentials.
        tracing::warn!("server's custom headers don't parse as JSON; ignoring them");
        return Vec::new();
    };
    let Some(entries) = value.as_object() else {
        tracing::warn!("server's custom headers aren't a JSON object; ignoring them");
        return Vec::new();
    };
    let mut headers: Vec<(String, String)> = entries
        .iter()
        .filter_map(|(name, value)| {
            let value = value.as_str()?;
            if name.eq_ignore_ascii_case("authorization") {
                tracing::warn!(header = %name, "custom header would override the app's authorization; ignoring it");
                return None;
            }
            if !abs_api::is_valid_header_name(name) {
                tracing::warn!(header = %name, "custom header name isn't valid HTTP; ignoring it");
                return None;
            }
            if !abs_api::is_valid_header_value(value) {
                tracing::warn!(header = %name, "custom header value isn't valid HTTP; ignoring it");
                return None;
            }
            Some((name.clone(), value.to_string()))
        })
        .collect();
    headers.sort();
    headers
}

/// Whether the server at `base_url` accepts a TCP connection — the reachability probe behind
/// the local-network-address policy. A raw TCP connect (250ms budget), not an HTTP request:
/// it asks "is anything listening there at all", which is all the policy needs, and works
/// before TLS is configured (a self-signed server would fail an HTTPS probe for exactly the
/// reason the user configured it). A `false` here just means "use the public URL" — the probe
/// never blocks a connection, it only chooses between two addresses.
pub async fn probe_reachable(base_url: &str) -> bool {
    const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str().map(str::to_string) else {
        return false;
    };
    let Some(port) = url.port_or_known_default() else {
        return false; // not an http/https-style URL with a known or explicit port
    };
    let attempt = tokio::net::TcpStream::connect((host.as_str(), port));
    tokio::time::timeout(PROBE_TIMEOUT, attempt).await.is_ok_and(|result| result.is_ok())
}

/// Save-time validation for the Connection page's local-address row: trims and normalizes
/// (trailing slash) like every other user-typed address in this app, allows an empty input
/// (clearing the address), and requires an absolute http(s) URL with a host for anything else —
/// the same shape `accounts`' login flow accepts for the public URL. Returns the canonical
/// form to store; the error is the dialog's user-facing message.
pub fn validate_local_address(address: &str) -> Result<String, String> {
    let trimmed = address.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    match reqwest::Url::parse(trimmed) {
        Ok(url) if url.has_host() && matches!(url.scheme(), "http" | "https") => Ok(trimmed.to_string()),
        _ => Err(format!("{trimmed:?} is not an http(s) address — e.g. http://192.168.1.50:13378")),
    }
}

/// Save-time validation for the Connection page's client-certificate row — the exact same
/// read+parse a mint performs, so a certificate the dialog accepts cannot then fail at mint
/// time (and vice versa). The error is the dialog's user-facing message.
pub fn validate_client_cert(path: &std::path::Path, password: Option<&str>) -> Result<(), String> {
    abs_api::validate_client_cert_file(path, password).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn unparsable_custom_headers_are_never_logged_verbatim() {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer({
                let capture = capture.clone();
                move || capture.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            assert!(parse_custom_headers("{X-Proxy-Auth: proxy-secret").is_empty());
            assert!(parse_custom_headers(r#"["proxy-secret"]"#).is_empty());
        });
        let logged = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("ignoring them"), "the warning is still emitted: {logged}");
        assert!(!logged.contains("proxy-secret"), "header contents leaked into the log: {logged}");
    }

    use super::*;

    fn server(url: &str) -> Server {
        Server {
            id: "server-1".to_string(),
            url: url.to_string(),
            custom_headers_json: "{}".to_string(),
            disable_ssl_verify: false,
            client_cert_path: None,
            client_cert_password: None,
            local_network_address: None,
            user_agent: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn resolve_without_a_local_address_uses_the_public_url() {
        let target = ConnectionTarget::resolve(&server("https://abs.example.org"), Some(true));
        assert_eq!(target.base_url, "https://abs.example.org");
        assert_eq!(target.options, abs_api::ConnectionOptions::default());
    }

    #[test]
    fn resolve_prefers_a_reachable_local_address() {
        let mut row = server("https://abs.example.org");
        row.local_network_address = Some("http://192.168.1.50:13378".to_string());
        let target = ConnectionTarget::resolve(&row, Some(true));
        assert_eq!(target.base_url, "http://192.168.1.50:13378", "reachable local address wins");
        // …and falls back when it isn't reachable, or when the probe couldn't tell.
        assert_eq!(ConnectionTarget::resolve(&row, Some(false)).base_url, "https://abs.example.org");
        assert_eq!(ConnectionTarget::resolve(&row, None).base_url, "https://abs.example.org");
    }

    #[test]
    fn resolve_ignores_a_blank_local_address() {
        let mut row = server("https://abs.example.org");
        row.local_network_address = Some("   ".to_string());
        assert_eq!(ConnectionTarget::resolve(&row, Some(true)).base_url, "https://abs.example.org");
    }

    #[test]
    fn resolve_trims_and_normalizes_the_local_address() {
        let mut row = server("https://abs.example.org");
        row.local_network_address = Some("  http://192.168.1.50:13378/  ".to_string());
        assert_eq!(ConnectionTarget::resolve(&row, Some(true)).base_url, "http://192.168.1.50:13378");
    }

    #[test]
    fn resolve_carries_all_transport_settings() {
        let mut row = server("https://abs.example.org");
        row.custom_headers_json = r#"{"X-Auth":"secret"}"#.to_string();
        row.disable_ssl_verify = true;
        row.client_cert_path = Some("/certs/client.p12".to_string());
        row.client_cert_password = Some("pw".to_string());
        row.user_agent = Some("  MyAgent/1.0  ".to_string());

        let target = ConnectionTarget::resolve(&row, None);
        assert_eq!(target.options.extra_headers, vec![("X-Auth".to_string(), "secret".to_string())]);
        assert!(target.options.disable_ssl_verify);
        assert_eq!(target.options.client_cert_path.as_deref(), Some(std::path::Path::new("/certs/client.p12")));
        assert_eq!(target.options.client_cert_password.as_deref(), Some("pw"));
        assert_eq!(target.options.user_agent.as_deref(), Some("MyAgent/1.0"), "trimmed");
        assert_eq!(target.user_agent(), "MyAgent/1.0", "playback sends the override too");
    }

    #[test]
    fn resolve_drops_blank_transport_settings() {
        let mut row = server("https://abs.example.org");
        row.client_cert_path = Some("  ".to_string());
        row.client_cert_password = Some("".to_string());
        row.user_agent = Some("".to_string());
        let target = ConnectionTarget::resolve(&row, None);
        assert_eq!(target.options.client_cert_path, None);
        assert_eq!(target.options.client_cert_password, None);
        assert_eq!(target.options.user_agent, None);
        assert_eq!(target.user_agent(), abs_api::default_user_agent(), "playback falls back to the app's own agent");
    }

    #[test]
    fn direct_is_a_default_options_target() {
        let target = ConnectionTarget::direct("http://localhost:1");
        assert_eq!(target.base_url, "http://localhost:1");
        assert_eq!(target.options, abs_api::ConnectionOptions::default());
    }

    #[test]
    fn track_url_builds_the_authenticated_stream_url() {
        let target = ConnectionTarget::direct("https://abs.example.org");
        assert_eq!(
            target.track_url("item-1", "ino-7", "tok"),
            "https://abs.example.org/api/items/item-1/file/ino-7?token=tok"
        );
    }

    #[test]
    fn parse_custom_headers_sorts_and_normalizes() {
        let headers = parse_custom_headers(r#"{"Z-Header":"1","A-Header":"2"}"#);
        assert_eq!(headers, vec![("A-Header".to_string(), "2".to_string()), ("Z-Header".to_string(), "1".to_string())]);
    }

    #[test]
    fn parse_custom_headers_tolerates_garbage() {
        assert!(parse_custom_headers("not json at all").is_empty());
        assert!(parse_custom_headers(r#"["an","array"]"#).is_empty());
        assert!(parse_custom_headers(r#"{"nonstring": 3}"#).is_empty(), "a non-string value is skipped");
        assert!(parse_custom_headers("{}").is_empty());
    }

    #[test]
    fn parse_custom_headers_skips_authorization_and_invalid_headers() {
        let headers = parse_custom_headers(r#"{"authorization":"stolen","bad name":"v","X-Good":"v","bad\nname":"v"}"#);
        assert_eq!(headers, vec![("X-Good".to_string(), "v".to_string())], "authorization, space-in-name and control-char-in-name are all dropped");
    }

    #[test]
    fn validate_custom_headers_text_accepts_and_canonicalizes() {
        let json = validate_custom_headers_text("X-Auth: secret\n\n  Z-Header:  spaced  \n").unwrap();
        assert_eq!(json, r#"{"X-Auth":"secret","Z-Header":"spaced"}"#, "blank lines skipped, values trimmed, keys sorted");
    }

    #[test]
    fn validate_custom_headers_text_rejects_each_problem() {
        assert!(matches!(validate_custom_headers_text("no colon here"), Err(CustomHeadersError::MissingColon { line: 1 })));
        assert!(matches!(validate_custom_headers_text("bad name: v"), Err(CustomHeadersError::InvalidName { line: 1, .. })));
        assert!(matches!(validate_custom_headers_text("X-Ok: no colon\u{7}here"), Err(CustomHeadersError::InvalidValue { line: 1 })));
        assert!(matches!(
            validate_custom_headers_text("Authorization: Bearer stolen"),
            Err(CustomHeadersError::AuthorizationReserved)
        ));
        assert!(matches!(
            validate_custom_headers_text("authorization: Bearer stolen"),
            Err(CustomHeadersError::AuthorizationReserved)
        ));
        assert!(validate_custom_headers_text("   \n \n").is_ok(), "nothing to validate is not an error — it clears the headers");
    }

    #[tokio::test]
    async fn probe_reachable_is_true_for_a_listening_port_and_false_for_a_closed_one() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe_reachable(&format!("http://127.0.0.1:{port}")).await, "a live listener is reachable");

        assert!(!probe_reachable("http://127.0.0.1:1").await, "a closed port is not");
        assert!(!probe_reachable("not a url").await, "an unparseable address can't be probed");
    }

    #[test]
    fn validate_local_address_accepts_normalizes_and_allows_clearing() {
        assert_eq!(validate_local_address("  http://192.168.1.50:13378/ ").unwrap(), "http://192.168.1.50:13378");
        assert_eq!(validate_local_address("   ").unwrap(), "", "empty input clears the address");
        assert!(validate_local_address("192.168.1.50").is_err(), "a bare host is not a URL");
        assert!(validate_local_address("ftp://192.168.1.50").is_err(), "non-http schemes are rejected");
        assert!(validate_local_address("https://").is_err(), "a scheme with no host at all is rejected");
    }

    #[test]
    fn validate_client_cert_reports_a_missing_file() {
        let err = validate_client_cert(std::path::Path::new("/nonexistent/cert.p12"), Some("pw")).unwrap_err();
        assert!(err.contains("/nonexistent/cert.p12"), "the message names the file: {err}");
    }
}
