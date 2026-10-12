//! The Connection page's options (custom headers, client certificate, "disable SSL
//! verification", local network address) and the ways they combine, exercised against real TLS
//! sockets rather than mocks: `support/fake_abs.py` plays an Audiobookshelf server behind a
//! self-signed certificate, a mutual-TLS gate and/or a shared-secret header gate.
//!
//! `#[ignore]`d because it needs `python3` and `openssl` on the PATH. Run with:
//!   cargo test -p abs-core --test connection_options -- --ignored --test-threads=4 --nocapture
//!
//! Options chosen on the login screen's Advanced section reach a server that has no row yet as
//! `ConnectionSettings` (see `a_new_server_that_needs_every_option_can_be_added_from_the_login_screen`).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use abs_api::LoginError;
use abs_core::accounts::{ClientCertificate, ConnectionSettings, SignIn};
use abs_core::auth::Session;
use abs_core::error::CoreError;
use abs_core::passwords::NoPasswords;
use abs_storage::repo::{accounts, servers};
use sqlx::SqlitePool;

const RIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/fake_abs.py");
const GATE: &str = "X-Proxy-Auth: s3cret";

struct Certs {
    dir: tempfile::TempDir,
}

impl Certs {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("python3").args(["-I", RIG, "certs"]).arg(dir.path()).status().expect("python3 and openssl are needed");
        assert!(status.success(), "generating the test certificates failed");
        Certs { dir }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn client_p12(&self) -> String {
        self.path().join("client.p12").to_string_lossy().into_owned()
    }
    fn other_p12(&self) -> String {
        self.path().join("other.p12").to_string_lossy().into_owned()
    }
}

struct Rig {
    child: Child,
    port: u16,
    log: PathBuf,
    tls: bool,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn start(certs: Option<&Certs>, mtls: bool, header: Option<&str>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("requests.log");
        let mut command = Command::new("python3");
        command.args(["-I", RIG, "serve", "--log"]).arg(&log);
        if let Some(certs) = certs {
            command.arg("--tls").arg(certs.path());
        }
        if mtls {
            command.arg("--mtls");
        }
        if let Some(header) = header {
            command.args(["--header", header]);
        }
        let mut child = command.stdout(Stdio::piped()).spawn().expect("python3 is needed");
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
        let port = line.trim().strip_prefix("READY ").unwrap_or_else(|| panic!("the rig didn't start: {line:?}")).parse().unwrap();
        Rig { child, port, log, tls: certs.is_some(), _dir: dir }
    }

    fn url(&self) -> String {
        format!("{}://127.0.0.1:{}", if self.tls { "https" } else { "http" }, self.port)
    }

    /// Every request that got past the TLS handshake, in order.
    fn requests(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn requests_to(&self, path: &str) -> Vec<serde_json::Value> {
        self.requests().into_iter().filter(|r| r["path"] == path).collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A URL nothing listens on (the port was just released).
fn dead_url(scheme: &str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("{scheme}://127.0.0.1:{port}")
}

async fn pool() -> (SqlitePool, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = abs_storage::connect_and_migrate(&tmp.path().join("db.sqlite3")).await.unwrap();
    (pool, tmp)
}

/// What the Connection page's rows write, for a server that already has a row.
#[derive(Default)]
struct Options<'a> {
    disable_ssl_verify: bool,
    header: Option<&'a str>,
    client_cert: Option<(&'a str, &'a str)>,
    local_address: Option<&'a str>,
}

async fn apply(pool: &SqlitePool, server_id: &str, options: &Options<'_>) {
    if options.disable_ssl_verify {
        servers::set_disable_ssl_verify(pool, server_id, true).await.unwrap();
    }
    if let Some(header) = options.header {
        let json = abs_core::connection::validate_custom_headers_text(header).unwrap();
        servers::set_custom_headers_json(pool, server_id, &json).await.unwrap();
    }
    if let Some((path, password)) = options.client_cert {
        servers::set_client_cert_path(pool, server_id, Some(path)).await.unwrap();
        servers::set_client_cert_password(pool, server_id, Some(password)).await.unwrap();
    }
    if let Some(address) = options.local_address {
        servers::set_local_network_address(pool, server_id, Some(address)).await.unwrap();
    }
}

async fn login(pool: &SqlitePool, url: &str) -> Result<abs_core::accounts::AddedAccount, CoreError> {
    abs_core::accounts::add_server_and_login(pool, url, "test", "test", &NoPasswords, false).await
}

/// `Result::expect_err` without needing `AddedAccount: Debug`.
fn rejected<T>(result: Result<T, CoreError>, why: &str) -> CoreError {
    match result {
        Ok(_) => panic!("expected a failure: {why}"),
        Err(err) => err,
    }
}

async fn session_for(pool: &SqlitePool, added: &abs_core::accounts::AddedAccount) -> Session {
    let server = servers::get(pool, &added.server_id).await.unwrap();
    let account = accounts::get(pool, &added.account_id).await.unwrap();
    Session::new(pool.clone(), &server, &account)
}

/// A server row configured before any login, the way the Connection page can only do once a
/// server exists (here: made by a first login that the options allowed, then signed out).
async fn row_with(pool: &SqlitePool, url: &str, options: &Options<'_>) -> String {
    let server_id = servers::add(pool, url).await.unwrap();
    apply(pool, &server_id, options).await;
    server_id
}

// ---------------------------------------------------------------------------------------------
// Baseline
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn a_plain_http_server_logs_in_and_lists_libraries() {
    let rig = Rig::start(None, false, None);
    let (pool, _tmp) = pool().await;
    let added = login(&pool, &rig.url()).await.expect("plain login");
    let session = session_for(&pool, &added).await;
    let libraries = session.api_client().await.unwrap().get_libraries().await.unwrap().into_inner().libraries;
    assert_eq!(libraries.len(), 1);
    let login_request = &rig.requests_to("/login")[0];
    assert!(login_request["user_agent"].as_str().unwrap().starts_with("Audiobooklet/"), "{login_request}");
}

// ---------------------------------------------------------------------------------------------
// Self-signed certificate / "Disable SSL verification"
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn a_self_signed_server_is_refused_by_default_and_nothing_is_stored() {
    let certs = Certs::new();
    let rig = Rig::start(Some(&certs), false, None);
    let (pool, _tmp) = pool().await;
    let err = rejected(login(&pool, &rig.url()).await, "an untrusted certificate must be refused");
    eprintln!("self-signed, verification on: {err:?}");
    assert!(matches!(err, CoreError::Login(LoginError::Tls(_))), "{err:?}");
    assert!(rig.requests().is_empty(), "no request may get through an unverified handshake");
    assert!(servers::list(&pool).await.unwrap().is_empty(), "a failed login stores nothing");
}

#[tokio::test]
#[ignore]
async fn a_self_signed_server_works_once_its_row_has_verification_off() {
    let certs = Certs::new();
    let rig = Rig::start(Some(&certs), false, None);
    let (pool, _tmp) = pool().await;
    row_with(&pool, &rig.url(), &Options { disable_ssl_verify: true, ..Options::default() }).await;
    let added = login(&pool, &rig.url()).await.expect("login with verification off");
    let session = session_for(&pool, &added).await;
    let libraries = session.api_client().await.unwrap().get_libraries().await.unwrap().into_inner().libraries;
    assert_eq!(libraries.len(), 1);
    let target = session.connection_target().await.unwrap();
    let refreshed = target.plain_client().unwrap().refresh("whatever").await.expect("token refresh honors the setting too");
    assert!(!refreshed.access_token.is_empty());
    assert!(rig.requests().iter().all(|r| r["tls"] == true));
}

/// A brand-new server that needs every option at once (a self-signed certificate, a header gate
/// and mutual TLS) is added with the options chosen on the login screen, which are then saved on
/// the server's row; without them nothing gets through and nothing is stored.
#[tokio::test]
#[ignore]
async fn a_new_server_that_needs_every_option_can_be_added_from_the_login_screen() {
    let certs = Certs::new();
    let rig = Rig::start(Some(&certs), true, Some(GATE));
    let (pool, _tmp) = pool().await;
    let err = rejected(login(&pool, &rig.url()).await, "default options can't reach this server");
    eprintln!("new server behind TLS+mTLS+header, no options: {err:?}");
    assert!(servers::list(&pool).await.unwrap().is_empty(), "a failed attempt stores nothing");

    let settings = ConnectionSettings {
        allow_untrusted_certificate: true,
        custom_headers_json: Some(abs_core::connection::validate_custom_headers_text(GATE).unwrap()),
        client_certificate: Some(ClientCertificate { path: certs.client_p12().into(), password: Some("pw".into()) }),
    };
    let sign_in = SignIn::Password { username: "test", password: "test" };
    let added = abs_core::accounts::add_server_and_sign_in(&pool, &rig.url(), &sign_in, &NoPasswords, false, &settings).await.expect("with the options chosen");

    let row = servers::get(&pool, &added.server_id).await.unwrap();
    assert!(row.disable_ssl_verify);
    assert_eq!(row.custom_headers_json, r#"{"X-Proxy-Auth":"s3cret"}"#);
    assert_eq!(row.client_cert_path.as_deref(), Some(certs.client_p12().as_str()));
    assert_eq!(row.client_cert_password.as_deref(), Some("pw"));
    // The saved options are all a later session needs.
    let session = session_for(&pool, &added).await;
    session.api_client().await.unwrap().get_libraries().await.expect("the row's options carry every later call");
}

// ---------------------------------------------------------------------------------------------
// Custom headers (a reverse proxy's shared secret)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn a_header_protected_server_refuses_a_login_without_the_header() {
    let rig = Rig::start(None, false, Some(GATE));
    let (pool, _tmp) = pool().await;
    let err = rejected(login(&pool, &rig.url()).await, "the proxy answers 403");
    eprintln!("header gate, no header: {err:?}");
    assert!(matches!(err, CoreError::Login(LoginError::Blocked(403))), "{err:?}");
    assert!(servers::list(&pool).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn the_custom_header_reaches_every_kind_of_request() {
    let rig = Rig::start(None, false, Some(GATE));
    let (pool, _tmp) = pool().await;
    row_with(&pool, &rig.url(), &Options { header: Some(GATE), ..Options::default() }).await;
    let added = login(&pool, &rig.url()).await.expect("login with the header");
    let session = session_for(&pool, &added).await;
    let target = session.connection_target().await.unwrap();
    let client = session.api_client().await.unwrap();
    client.get_libraries().await.unwrap();
    target.plain_client().unwrap().refresh("whatever").await.unwrap();
    let file = client.get_item_file_response("item-1", "ino-1", None).await.expect("a track download");
    drop(file);
    // The rig logs a request once its reply is written, which can be just after the client hangs up.
    for _ in 0..40 {
        if !rig.requests_to("/api/items/item-1/file/ino-1").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let requests = rig.requests();
    let paths: Vec<_> = requests.iter().map(|r| r["path"].as_str().unwrap().to_string()).collect();
    for expected in ["/login", "/api/libraries", "/auth/refresh", "/api/items/item-1/file/ino-1"] {
        assert!(paths.iter().any(|p| p == expected), "{expected} missing from {paths:?}");
    }
    assert!(requests.iter().all(|r| r["status"].as_i64().unwrap() < 400 && r["gate_header"] == "s3cret"), "{requests:#?}");
}

// ---------------------------------------------------------------------------------------------
// Client certificate (mutual TLS)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn an_mtls_server_needs_a_client_certificate_and_accepts_the_right_one() {
    let certs = Certs::new();
    let rig = Rig::start(Some(&certs), true, None);
    let (pool, _tmp) = pool().await;

    // No certificate at all (verification off, so only the missing identity is in the way).
    row_with(&pool, &rig.url(), &Options { disable_ssl_verify: true, ..Options::default() }).await;
    let err = rejected(login(&pool, &rig.url()).await, "the handshake needs a client certificate");
    eprintln!("mTLS, no client certificate: {err:?}");
    // The server's alert normally arrives as a TLS error; a connection reset that beats it can't be
    // told apart from a network failure.
    assert!(matches!(err, CoreError::Login(LoginError::ClientCertificate(_) | LoginError::Network(_))), "{err:?}");
    assert!(rig.requests().is_empty());

    // A certificate from a CA the server doesn't trust.
    let server = servers::list(&pool).await.unwrap().remove(0);
    apply(&pool, &server.id, &Options { client_cert: Some((&certs.other_p12(), "pw")), ..Options::default() }).await;
    let err = rejected(login(&pool, &rig.url()).await, "an untrusted client certificate is refused");
    eprintln!("mTLS, certificate from another CA: {err:?}");
    assert!(matches!(err, CoreError::Login(LoginError::ClientCertificate(_) | LoginError::Network(_))), "{err:?}");
    assert!(rig.requests().is_empty());

    // The right certificate, but the wrong export password: a clear mint-time error.
    apply(&pool, &server.id, &Options { client_cert: Some((&certs.client_p12(), "wrong")), ..Options::default() }).await;
    let err = rejected(login(&pool, &rig.url()).await, "a bundle that can't be opened");
    eprintln!("mTLS, wrong bundle password: {err:?}");
    assert!(matches!(err, CoreError::ConnectionSetup(_)), "{err:?}");
    assert!(rig.requests().is_empty());

    // The right certificate and password.
    apply(&pool, &server.id, &Options { client_cert: Some((&certs.client_p12(), "pw")), ..Options::default() }).await;
    let added = login(&pool, &rig.url()).await.expect("login with the right client certificate");
    let session = session_for(&pool, &added).await;
    session.api_client().await.unwrap().get_libraries().await.unwrap();
    let requests = rig.requests();
    assert!(requests.len() >= 2 && requests.iter().all(|r| r["client_cert_cn"] == "test client"), "{requests:#?}");
}

#[tokio::test]
#[ignore]
async fn a_client_certificate_file_that_disappears_fails_loudly_not_silently() {
    let certs = Certs::new();
    let rig = Rig::start(Some(&certs), true, None);
    let (pool, _tmp) = pool().await;
    let copy = certs.path().join("moved.p12");
    std::fs::copy(certs.client_p12(), &copy).unwrap();
    let options = Options { disable_ssl_verify: true, client_cert: Some((copy.to_str().unwrap(), "pw")), ..Options::default() };
    row_with(&pool, &rig.url(), &options).await;
    let added = login(&pool, &rig.url()).await.unwrap();
    let session = session_for(&pool, &added).await;
    session.api_client().await.unwrap().get_libraries().await.unwrap();
    std::fs::remove_file(&copy).unwrap();
    // The session reuses the client it already built (identity in memory), so it keeps working…
    session.api_client().await.unwrap().get_libraries().await.expect("the cached client still has its identity");
    // …but anything that builds a client afresh (a restart, a new session) says what is wrong.
    let fresh = session_for(&pool, &added).await;
    let Err(err) = fresh.api_client().await else { panic!("a new mint must fail") };
    eprintln!("client certificate file gone: {err:?}");
    assert!(err.to_string().contains("client certificate"), "{err}");
}

// ---------------------------------------------------------------------------------------------
// Local network address
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn the_local_address_is_used_when_reachable_and_the_public_url_otherwise() {
    let public = Rig::start(None, false, None);
    let local = Rig::start(None, false, None);
    let (pool, _tmp) = pool().await;
    let local_url = local.url();
    row_with(&pool, &public.url(), &Options { local_address: Some(&local_url), ..Options::default() }).await;
    let added = login(&pool, &public.url()).await.expect("login goes to the public URL");
    assert_eq!(public.requests_to("/login").len(), 1);
    assert_eq!(local.requests().len(), 0, "login never uses the local address");

    let session = session_for(&pool, &added).await;
    assert_eq!(session.connection_target().await.unwrap().base_url, local_url);
    session.api_client().await.unwrap().get_libraries().await.unwrap();
    assert_eq!(local.requests_to("/api/libraries").len(), 1, "later calls take the local address");
    assert!(public.requests_to("/api/libraries").is_empty());

    // The home network is left: nothing listens on the local address any more. (The probe result
    // is cached per session for a short while, so a fresh session stands in for "later".)
    drop(local);
    let session = session_for(&pool, &added).await;
    assert_eq!(session.connection_target().await.unwrap().base_url, public.url());
    session.api_client().await.unwrap().get_libraries().await.unwrap();
    assert_eq!(public.requests_to("/api/libraries").len(), 1);
}

#[tokio::test]
#[ignore]
async fn a_local_address_works_with_the_server_s_other_options() {
    let certs = Certs::new();
    let local = Rig::start(Some(&certs), true, Some(GATE));
    let (pool, _tmp) = pool().await;
    let (public_url, local_url) = (dead_url("https"), local.url());
    let options = Options {
        disable_ssl_verify: true,
        header: Some(GATE),
        client_cert: Some((&certs.client_p12(), "pw")),
        local_address: Some(&local_url),
    };
    let server_id = row_with(&pool, &public_url, &options).await;
    // The public address can't be reached from here, so the account is created directly (the
    // login screen only ever talks to the public address).
    let account_id = accounts::add(&pool, &server_id, "test", &fake_jwt(), None).await.unwrap();
    let added = abs_core::accounts::AddedAccount { server_id, account_id };
    let session = session_for(&pool, &added).await;

    let target = session.connection_target().await.unwrap();
    assert_eq!(target.base_url, local_url);
    let client = session.api_client().await.unwrap();
    client.get_libraries().await.expect("TLS + client certificate + header, over the local address");
    drop(client.get_item_file_response("item-1", "ino-1", None).await.expect("a track download"));
    let requests = local.requests();
    assert!(requests.iter().all(|r| r["client_cert_cn"] == "test client" && r["gate_header"] == "s3cret"), "{requests:#?}");
}

// ---------------------------------------------------------------------------------------------
// Everything at once, on the public address
// ---------------------------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn self_signed_plus_mtls_plus_header_gate_logs_in_when_all_options_are_set() {
    let certs = Certs::new();
    let rig = Rig::start(Some(&certs), true, Some(GATE));
    let (pool, _tmp) = pool().await;
    let options = Options { disable_ssl_verify: true, header: Some(GATE), client_cert: Some((&certs.client_p12(), "pw")), local_address: None };
    row_with(&pool, &rig.url(), &options).await;
    let added = login(&pool, &rig.url()).await.expect("all three options together");
    let session = session_for(&pool, &added).await;
    session.api_client().await.unwrap().get_libraries().await.unwrap();

    // Dropping any one of the three breaks it, each in its own way.
    for (name, without) in [
        ("verification", Options { disable_ssl_verify: false, header: Some(GATE), client_cert: Some((&certs.client_p12(), "pw")), local_address: None }),
        ("header", Options { disable_ssl_verify: true, header: None, client_cert: Some((&certs.client_p12(), "pw")), local_address: None }),
        ("certificate", Options { disable_ssl_verify: true, header: Some(GATE), client_cert: None, local_address: None }),
    ] {
        let (pool, _tmp) = self::pool().await;
        row_with(&pool, &rig.url(), &without).await;
        if name == "certificate" {
            servers::set_client_cert_path(&pool, &servers::list(&pool).await.unwrap()[0].id, None).await.unwrap();
        }
        let err = rejected(login(&pool, &rig.url()).await, name);
        eprintln!("without {name}: {err:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------------------------

/// The login screen has an "API Token" toggle that only shows "API tokens aren't supported yet".
/// The engine underneath does cope with such a credential once it is stored: a token with no
/// refresh token is used as-is, with no refresh attempts.
#[tokio::test]
#[ignore]
async fn an_account_holding_an_api_key_instead_of_a_login_works_without_refreshing() {
    let rig = Rig::start(None, false, None);
    let (pool, _tmp) = pool().await;
    let server_id = servers::add(&pool, &rig.url()).await.unwrap();
    let account_id = accounts::add(&pool, &server_id, "api-user", "api-key-token", None).await.unwrap();
    let session = session_for(&pool, &abs_core::accounts::AddedAccount { server_id, account_id }).await;
    assert_eq!(session.access_token().await, "api-key-token");
    session.api_client().await.unwrap().get_libraries().await.expect("the key is sent as a bearer token");
    assert!(rig.requests_to("/auth/refresh").is_empty());
}

/// API-key sign-in on every kind of server: the key goes out as a bearer token, `/api/me` names the
/// user, and the account has no refresh token.
#[tokio::test]
#[ignore]
async fn an_api_key_signs_in_on_plain_header_gated_self_signed_and_mutual_tls_servers() {
    let certs = Certs::new();
    let plain = Rig::start(None, false, None);
    let gated = Rig::start(None, false, Some(GATE));
    let self_signed = Rig::start(Some(&certs), false, None);
    let everything = Rig::start(Some(&certs), true, Some(GATE));
    let header = ConnectionSettings { custom_headers_json: Some(abs_core::connection::validate_custom_headers_text(GATE).unwrap()), ..ConnectionSettings::default() };
    let untrusted = ConnectionSettings { allow_untrusted_certificate: true, ..ConnectionSettings::default() };
    let all = ConnectionSettings {
        allow_untrusted_certificate: true,
        custom_headers_json: header.custom_headers_json.clone(),
        client_certificate: Some(ClientCertificate { path: certs.client_p12().into(), password: Some("pw".into()) }),
    };
    for (name, rig, settings) in [("plain", &plain, ConnectionSettings::default()), ("header gate", &gated, header), ("self-signed", &self_signed, untrusted), ("everything", &everything, all)] {
        let (pool, _tmp) = pool().await;
        let key = format!("{}.key.sig", fake_jwt().split('.').next().unwrap());
        let added = abs_core::accounts::add_server_and_sign_in(&pool, &rig.url(), &SignIn::ApiKey(&format!(" {key} ")), &NoPasswords, false, &settings)
            .await
            .unwrap_or_else(|err| panic!("{name}: {err:?}"));
        let account = accounts::get(&pool, &added.account_id).await.unwrap();
        assert_eq!(account.username, "test", "{name}: named by /api/me");
        assert_eq!(account.token, key, "{name}: the key, trimmed");
        assert_eq!(account.refresh_token, None, "{name}: nothing to refresh");
        let session = session_for(&pool, &added).await;
        session.api_client().await.unwrap().get_libraries().await.unwrap_or_else(|err| panic!("{name}: {err:?}"));
        assert!(rig.requests_to("/auth/refresh").is_empty(), "{name}");
        assert_eq!(rig.requests_to("/api/me").len(), 1, "{name}");
    }
}

/// Signing in again with a key: the same user keeps the account; a server that doesn't accept the
/// key stores nothing; a key with characters a header can't carry says so.
#[tokio::test]
#[ignore]
async fn an_api_key_relogin_and_failures() {
    let rig = Rig::start(None, false, Some(GATE));
    let (pool, tmp) = pool().await;
    let paths = abs_storage::AppPaths::rooted_at(tmp.path().join("data"), tmp.path().join("cache"), tmp.path().join("state"));
    let header = ConnectionSettings { custom_headers_json: Some(abs_core::connection::validate_custom_headers_text(GATE).unwrap()), ..ConnectionSettings::default() };
    let key = fake_jwt();
    let added = abs_core::accounts::add_server_and_sign_in(&pool, &rig.url(), &SignIn::ApiKey(&key), &NoPasswords, false, &header).await.unwrap();

    // Without the header the proxy refuses the key check: Blocked, nothing changes.
    let err = rejected(abs_core::accounts::add_server_and_sign_in(&pool, &dead_url("http"), &SignIn::ApiKey(&key), &NoPasswords, false, &ConnectionSettings::default()).await, "nothing listens there");
    eprintln!("key, dead address: {err:?}");
    assert_eq!(servers::list(&pool).await.unwrap().len(), 1);

    let seed = abs_core::accounts::ReloginSeed { server_id: added.server_id.clone(), account_id: added.account_id.clone(), url: rig.url(), username: "test".into() };
    let username = abs_core::accounts::api_key_username(&pool, &seed, &rig.url(), &key, &ConnectionSettings::default()).await.expect("the saved header applies to a same-server re-login");
    assert_eq!(username, "test");
    let again = abs_core::accounts::relogin_with(&pool, &paths, &seed, &rig.url(), &SignIn::ApiKey(&key), &NoPasswords, false, &ConnectionSettings::default()).await.unwrap();
    assert_eq!(again.account_id, added.account_id, "the same user on the same server keeps the account");

    let err = rejected(abs_core::accounts::add_server_and_sign_in(&pool, &rig.url(), &SignIn::ApiKey("bad\nkey"), &NoPasswords, false, &header).await, "an unusable key");
    assert!(matches!(err, CoreError::ConnectionSetup(_)), "{err:?}");
}

fn fake_jwt() -> String {
    use base64::Engine;
    let b64 = |s: String| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
    let exp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600;
    format!("{}.{}.sig", b64(r#"{"alg":"HS256"}"#.into()), b64(format!(r#"{{"exp":{exp}}}"#)))
}
